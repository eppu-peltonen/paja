//! Jono: the work queue. Work comes from GitHub issues with the repository's work label
//! (`.paja/labels`); `sync` imports them
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

/// The repository's queue labels, read from the default branch.
pub const LABELS_FILE: &str = ".paja/labels";

/// The labels a repository names for its queue (`.paja/labels` on the default branch): `work`
/// puts an issue in Jono, `follow-up` marks the non-blocking findings Mestari files, and
/// `needs-answer` marks an escalated item until `paja work answer` requeues it. Without the
/// file they are `factory`, `factory-follow-up` and `factory-needs-answer`, which don't name
/// any project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Labels {
    pub work: String,
    pub follow_up: String,
    pub needs_answer: String,
}

impl Default for Labels {
    fn default() -> Self {
        Labels {
            work: "factory".into(),
            follow_up: "factory-follow-up".into(),
            needs_answer: "factory-needs-answer".into(),
        }
    }
}

impl Labels {
    /// Read the file's text: `work <label>`, `follow-up <label>` and `needs-answer <label>`
    /// lines (any may be left out), `#` starts a comment line. Anything else is an error naming the line and the fix.
    pub fn parse(text: &str) -> Result<Self> {
        let mut labels = Labels::default();
        let (mut work, mut follow_up, mut needs_answer) = (false, false, false);
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = format!(
                "{LABELS_FILE} on the default branch, line {}: {line:?}",
                i + 1
            );
            let (key, value) = line
                .split_once(char::is_whitespace)
                .map(|(key, value)| (key, value.trim()))
                .unwrap_or((line, ""));
            if value.is_empty() || value.starts_with('-') || value.contains([',', '\\']) {
                bail!(
                    "{at}: write it as `{key} <label>`, a label that doesn't start with `-` and \
                     has no comma"
                );
            }
            let (slot, seen) = match key {
                "work" => (&mut labels.work, &mut work),
                "follow-up" => (&mut labels.follow_up, &mut follow_up),
                "needs-answer" => (&mut labels.needs_answer, &mut needs_answer),
                _ => bail!("{at}: unknown key {key:?}; use `work`, `follow-up` or `needs-answer`"),
            };
            if std::mem::replace(seen, true) {
                bail!("{at}: `{key}` is named twice; keep one line");
            }
            *slot = value.to_string();
        }
        if labels.work == labels.follow_up {
            bail!(
                "{LABELS_FILE} on the default branch gives `work` and `follow-up` the same label {:?}; they \
                 must differ, or follow-ups would queue themselves",
                labels.work
            );
        }
        if labels.needs_answer == labels.work || labels.needs_answer == labels.follow_up {
            bail!(
                "{LABELS_FILE} on the default branch gives `needs-answer` the label {:?}, which \
                 `work` or `follow-up` already uses; give it its own, since answering removes it",
                labels.needs_answer
            );
        }
        Ok(labels)
    }

    /// The labels on the default branch of the checkout `ok` proved safe (see
    /// [`crate::ahjo::show_on_default_branch`]). A repository without the file, or without a
    /// default branch, gets the defaults. Never read from the branch checked out, which is the
    /// change under review.
    fn of_default_branch(ok: &Untampered) -> Result<Self> {
        match crate::ahjo::show_on_default_branch(ok, LABELS_FILE)? {
            Some(text) => Self::parse(&text),
            None => Ok(Self::default()),
        }
    }
}

/// The repository's queue settings, read from the default branch.
pub const QUEUE_FILE: &str = ".paja/queue";

/// Queue settings (`.paja/queue` on the default branch). Without the file, or without a line,
/// every setting has its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueRules {
    /// Whether Jono takes follow-up issues when no work item is ready (`follow-ups = on|off`).
    pub follow_ups: bool,
}

impl Default for QueueRules {
    fn default() -> Self {
        QueueRules { follow_ups: true }
    }
}

impl QueueRules {
    /// Read the file's text: `follow-ups = on` or `follow-ups = off`, `#` starts a comment
    /// line. Anything else is an error naming the line and the fix.
    pub fn parse(text: &str) -> Result<Self> {
        let mut rules = QueueRules::default();
        let mut seen = false;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = format!(
                "{QUEUE_FILE} on the default branch, line {}: {line:?}",
                i + 1
            );
            let (key, value) = line
                .split_once('=')
                .map(|(k, v)| (k.trim(), v.trim()))
                .unwrap_or((line, ""));
            if key != "follow-ups" {
                bail!("{at}: unknown setting; the only one is `follow-ups = on` or `follow-ups = off`");
            }
            if std::mem::replace(&mut seen, true) {
                bail!("{at}: `follow-ups` is set twice; keep one line");
            }
            rules.follow_ups = match value {
                "on" => true,
                "off" => false,
                _ => bail!("{at}: write `follow-ups = on` or `follow-ups = off`"),
            };
        }
        Ok(rules)
    }

    fn of_default_branch(ok: &Untampered) -> Result<Self> {
        match crate::ahjo::show_on_default_branch(ok, QUEUE_FILE)? {
            Some(text) => Self::parse(&text),
            None => Ok(Self::default()),
        }
    }
}

/// How an item ranks in the queue: work items first, then follow-ups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Priority {
    #[default]
    #[serde(rename = "work")]
    Work,
    /// An issue with the follow-up label (and not the work label), taken only when no work item
    /// is ready.
    #[serde(rename = "follow-up")]
    FollowUp,
}

impl Priority {
    fn is_work(&self) -> bool {
        *self == Priority::Work
    }
}

/// Jono's order: by repository, work items before follow-ups, then oldest first (the sort is
/// stable and items arrive by number).
fn queue_order(items: &mut [WorkItem]) {
    items.sort_by(|a, b| (&a.repo, a.priority).cmp(&(&b.repo, b.priority)));
}

pub const EV_IMPORTED: &str = "work.imported";
pub const EV_CHANGED: &str = "work.changed";
pub const EV_CLOSED: &str = "work.closed";
pub const EV_REOPENED: &str = "work.reopened";
pub const EV_STATUS: &str = "work.status";
/// A person answered an escalated item's question.
pub const EV_ANSWERED: &str = "work.answered";
/// A sync found which of an item's dependencies are still open: `{"number", "on": [...]}`.
pub const EV_WAITING: &str = "work.waiting";
/// A repository's first appearance in a log that holds events from before work items carried
/// one: `{"repo"}`. It pins those older events to that repository (see [`legacy_repo`]).
pub const EV_SYNCED: &str = "work.synced";

/// An item's identity: its repository (`owner/name`) and its issue number.
pub type Key = (String, u64);

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
    /// The GitHub repository the issue belongs to, as `owner/name`.
    #[serde(default)]
    pub repo: String,
    /// The GitHub issue number, unique within `repo`.
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    /// Whether the issue is open and has the work label. A closed issue keeps its status, so an
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
    /// `work` for an issue with the work label, `follow-up` for one that only has the follow-up
    /// label. Jono starts every ready `work` item before any `follow-up`.
    #[serde(default)]
    pub priority: Priority,
}

impl WorkItem {
    /// The name of the item's Ahjo environment (its branch is `paja/<name>`): the repository's
    /// name and a short hash of `owner/name`, so two repositories' item #5 never share a
    /// worktree, a branch or a port block. At most 40 characters (Ahjo's limit).
    pub fn env_name(&self) -> String {
        env_name(&self.repo, self.number)
    }
}

/// See [`WorkItem::env_name`].
pub fn env_name(repo: &str, number: u64) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo).to_ascii_lowercase();
    let name: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // What is left of the 40 after the hash, `-work-<number>` and the dash after the name.
    let room = 40usize.saturating_sub(6 + 6 + number.to_string().len() + 1);
    let name: String = name.chars().take(room.min(14)).collect();
    let name = name.trim_matches('-');
    // FNV-1a, which is stable across releases and platforms (unlike the std hasher).
    let hash = repo.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    let head = if name.is_empty() {
        String::new()
    } else {
        format!("{name}-")
    };
    format!("{head}{:06x}-work-{number}", hash & 0xff_ffff)
}

/// A person's answer to a question Paja asked about an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The question, as the escalation asked it, when it was known.
    #[serde(default)]
    pub question: Option<String>,
    pub answer: String,
}

/// An item's agent usage, totalled over every recorded step. A step whose result lacked a token
/// field or the cost counts in `unknown_steps` and still adds the fields it has, so while
/// `unknown_steps` is above zero each sum is a lower bound. A missing field adds nothing rather
/// than a zero it didn't see. A recorded step Jono can't read at all counts as unknown too, and
/// the sequence number of its `run.round` event goes in `unreadable_events`, so the bad record
/// can be found in the log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub steps: u64,
    pub unknown_steps: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable_events: Vec<u64>,
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
    /// Not in `gh`'s output: a sync sets it from the label it listed the issue by. Events
    /// record it only for follow-ups.
    #[serde(default, skip_serializing_if = "Priority::is_work")]
    pub priority: Priority,
}

/// What a sync changed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncReport {
    /// The repository that was synced, as `owner/name`.
    pub repo: String,
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

/// Whether `event` is one of the queue's or a run's, which carry a `repo` (the tamper events
/// use that name for a git directory instead, and belong to the checkout, not an item).
fn is_item_event(event: &Event) -> bool {
    (event.kind.starts_with("work.") || event.kind.starts_with("run."))
        && event.kind != crate::ahjo::EV_TAMPERED
        && event.kind != crate::ahjo::EV_TAMPER_CHECKED
}

/// The repository the events written before work items carried one belong to: the first
/// repository a `work.*` event names (the first `paja work sync` the log records writes one),
/// or `fallback`, the repository of the checkout the command runs in, when none does yet.
/// Replay applies this; nothing in `events.jsonl` is rewritten.
pub fn legacy_repo<'a>(events: &'a [Event], fallback: &'a str) -> &'a str {
    events
        .iter()
        .filter(|e| e.kind.starts_with("work."))
        .find_map(|e| e.data["repo"].as_str())
        .unwrap_or(fallback)
}

/// The repository `event` belongs to: the one it names, else `legacy` (see [`legacy_repo`]).
pub fn event_repo<'a>(event: &'a Event, legacy: &'a str) -> &'a str {
    event.data["repo"].as_str().unwrap_or(legacy)
}

/// The events of item `number` of `repo` (`fallback` as in [`legacy_repo`] is `repo` itself).
pub fn item_events<'a>(
    events: &'a [Event],
    repo: &'a str,
    number: u64,
) -> impl Iterator<Item = &'a Event> {
    let legacy = legacy_repo(events, repo);
    events
        .iter()
        .filter(move |e| e.data["number"] == number && event_repo(e, legacy) == repo)
}

/// Before the first event of a repository is written to a log that holds older, unkeyed ones,
/// record which repository those belong to (see [`legacy_repo`]), so a later write from another
/// checkout can't move them. Every writer calls this under the home lock.
fn pin_legacy(
    lock: &crate::home::HomeLock,
    log: &crate::events::EventLog,
    repo: &str,
) -> Result<()> {
    let events = log.read_all()?;
    let unkeyed = events
        .iter()
        .any(|e| is_item_event(e) && e.data["repo"].as_str().is_none());
    let pinned = events
        .iter()
        .any(|e| e.kind.starts_with("work.") && e.data["repo"].as_str().is_some());
    if unkeyed && !pinned {
        log.append(lock, EV_SYNCED, json!({ "repo": repo }))?;
    }
    Ok(())
}

/// The queue of one repository, derived by replaying the event log.
pub fn replay_repo(events: &[Event], repo: &str) -> Result<BTreeMap<u64, WorkItem>> {
    Ok(replay(events, repo)?
        .into_iter()
        .filter(|((r, _), _)| r == repo)
        .map(|((_, number), item)| (number, item))
        .collect())
}

/// Every repository's queue, derived by replaying the event log. Events written before they
/// carried a repository belong to [`legacy_repo`] (`fallback` is the checkout's repository).
pub fn replay(events: &[Event], fallback: &str) -> Result<BTreeMap<Key, WorkItem>> {
    let mut items: BTreeMap<Key, WorkItem> = BTreeMap::new();
    let bad = |event: &Event| format!("event {} ({}) has unexpected data", event.seq, event.kind);
    let legacy = legacy_repo(events, fallback);
    for event in events {
        let repo = event_repo(event, legacy);
        // The item an event is about.
        let key = |number: u64| (repo.to_string(), number);
        match event.kind.as_str() {
            EV_IMPORTED => {
                let issue: Issue =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                items.insert(
                    key(issue.number),
                    WorkItem {
                        repo: repo.to_string(),
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
                        priority: issue.priority,
                    },
                );
            }
            EV_CHANGED => {
                let issue: Issue =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&key(issue.number)) {
                    item.title = issue.title;
                    item.body = issue.body;
                    item.url = issue.url;
                    item.priority = issue.priority;
                }
            }
            EV_CLOSED | EV_REOPENED => {
                let n: Number =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&key(n.number)) {
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
                if let Some(item) = items.get_mut(&key(change.number)) {
                    item.status = change.status;
                    item.note = change.note;
                }
            }
            EV_WAITING => {
                let waiting: Waiting =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&key(waiting.number)) {
                    item.waiting_on = waiting.on;
                }
            }
            EV_ANSWERED => {
                let answered: Answered =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&key(answered.number)) {
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
                if let (Some(item), false) = (items.get_mut(&key(round.number)), steps.is_empty()) {
                    let usage = item.usage.get_or_insert_with(Usage::default);
                    for step in steps {
                        // Usage is informational: a step that can't be read counts as one whose
                        // usage is unknown.
                        match serde_json::from_value::<StepUsage>(step) {
                            Ok(step) => usage.add(&step),
                            Err(_) => {
                                usage.steps += 1;
                                usage.unknown_steps += 1;
                                if usage.unreadable_events.last() != Some(&event.seq) {
                                    usage.unreadable_events.push(event.seq);
                                }
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
    let recorded = waiting_recorded(events, legacy);
    for (key, item) in items.iter_mut() {
        if !recorded.contains(key) {
            item.waiting_on = dependencies(item.number, &item.body);
        }
    }
    Ok(items)
}

/// The items a `work.waiting` event was ever recorded for.
fn waiting_recorded(events: &[Event], legacy: &str) -> BTreeSet<Key> {
    events
        .iter()
        .filter(|e| e.kind == EV_WAITING)
        .filter_map(|e| {
            Some((
                event_repo(e, legacy).to_string(),
                e.data["number"].as_u64()?,
            ))
        })
        .collect()
}

/// The repository (`owner/name`) of the checkout at `path`, from `gh repo view`. Work items
/// belong to it.
pub fn repo_name(home: &Home, path: &Path) -> Result<String> {
    if !path.is_dir() {
        bail!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            path.display()
        );
    }
    match crate::ahjo::untampered(home, path) {
        Ok(ok) => name_of(&ok),
        // gh runs git, which a tampered configuration could abuse, so Paja asks it nothing
        // there. The queue stays readable (and an item can still be set) under the repository
        // the stop recorded.
        Err(err) if err.is::<crate::ahjo::Tampered>() => {
            let events = home.events().read_all()?;
            let key = crate::ahjo::repo_key(path)?;
            events
                .iter()
                .rev()
                .filter(|e| e.kind == crate::ahjo::EV_TAMPERED && e.data["repo"] == key)
                .find_map(|e| e.data["work_repo"].as_str().map(str::to_string))
                .ok_or(err)
        }
        Err(err) => Err(err),
    }
}

fn name_of(ok: &Untampered) -> Result<String> {
    let out = gh(ok, &["repo", "view", "--json", "nameWithOwner"])?;
    let name = serde_json::from_str::<Value>(&out)
        .ok()
        .and_then(|v| v["nameWithOwner"].as_str().map(str::to_string))
        .filter(|name| !name.is_empty());
    name.with_context(|| {
        format!(
            "`{} repo view` did not name the repository of {}; check `gh auth status` and that \
             this checkout has a GitHub remote",
            gh_program(),
            ok.path().display()
        )
    })
}

/// The work items of `repo` in the order Jono starts them: work items oldest first, then
/// follow-ups oldest first.
pub fn list(home: &Home, repo: &str) -> Result<Vec<WorkItem>> {
    let mut items: Vec<WorkItem> = replay_repo(&home.events().read_all()?, repo)?
        .into_values()
        .collect();
    queue_order(&mut items);
    Ok(items)
}

/// Every repository's work items, by repository and then in Jono's order (see [`list`]). `repo`
/// is the checkout's repository, which events from before items carried one may belong to.
pub fn list_all(home: &Home, repo: &str) -> Result<Vec<WorkItem>> {
    let mut items: Vec<WorkItem> = replay(&home.events().read_all()?, repo)?
        .into_values()
        .collect();
    queue_order(&mut items);
    Ok(items)
}

/// Every repository's work items, as [`list_all`], for a command run in `checkout`. Only a log
/// with events from before items carried a repository, and no `work.*` event yet that names
/// one, needs the checkout's repository; any other log is listed without asking `gh`, so this
/// works outside a GitHub checkout too.
pub fn list_all_from(home: &Home, checkout: &Path) -> Result<Vec<WorkItem>> {
    let events = home.events().read_all()?;
    let pinned = events
        .iter()
        .filter(|e| e.kind.starts_with("work."))
        .any(|e| e.data["repo"].is_string());
    let unplaced = events
        .iter()
        .any(|e| is_item_event(e) && !e.data["repo"].is_string());
    let fallback = if unplaced && !pinned {
        repo_name(home, checkout)?
    } else {
        String::new()
    };
    let mut items: Vec<WorkItem> = replay(&events, &fallback)?.into_values().collect();
    queue_order(&mut items);
    Ok(items)
}

pub fn get(home: &Home, repo: &str, number: u64) -> Result<WorkItem> {
    replay_repo(&home.events().read_all()?, repo)?
        .remove(&number)
        .with_context(|| no_item(repo, number))
}

/// One item as `paja work show` and its MCP tool print it: the item, plus `run` (its latest
/// run's round, step and finished rounds, see [`crate::mestari::live`]) while it is active.
pub fn show(home: &Home, repo: &str, number: u64) -> Result<Value> {
    let item = get(home, repo, number)?;
    let mut shown = serde_json::to_value(&item)?;
    if item.status == Status::Active {
        shown["run"] = serde_json::to_value(crate::mestari::live::live(home, repo, number)?)?;
    }
    Ok(shown)
}

fn no_item(repo: &str, number: u64) -> String {
    format!(
        "no work item #{number} in {repo}; run `paja work sync` in its checkout to import issues \
         with its work label (`{LABELS_FILE}` on the default branch)"
    )
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
pub fn waiting(home: &Home, repo: &str) -> Result<Option<String>> {
    let items = list(home, repo)?;
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

/// The next item of `repo` to work on: the oldest ready work item, else the oldest ready
/// follow-up. None when nothing is ready.
pub fn next(home: &Home, repo: &str) -> Result<Option<WorkItem>> {
    Ok(list(home, repo)?.into_iter().find(is_ready))
}

/// Take the next item of `repo` and mark it active in one step under the home lock, so two runs
/// never pick up the same item.
pub fn claim(home: &Home, repo: &str) -> Result<Option<WorkItem>> {
    let lock = home.lock()?;
    let log = home.events();
    pin_legacy(&lock, &log, repo)?;
    let mut items: Vec<WorkItem> = replay_repo(&log.read_all()?, repo)?.into_values().collect();
    queue_order(&mut items);
    let Some(mut item) = items.into_iter().find(is_ready) else {
        return Ok(None);
    };
    log.append(
        &lock,
        EV_STATUS,
        json!({ "repo": repo, "number": item.number, "status": Status::Active, "note": null }),
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
pub fn claim_for_run(home: &Home, repo: &str) -> Result<Option<RunClaim>> {
    claim_for_run_item(home, repo, None)
}

/// [`claim_for_run`], taking only item `only` when one is named (`paja run --item`).
pub fn claim_for_run_item(home: &Home, repo: &str, only: Option<u64>) -> Result<Option<RunClaim>> {
    let wanted = |item: &WorkItem| only.is_none_or(|n| n == item.number);
    let lock = home.lock()?;
    let log = home.events();
    pin_legacy(&lock, &log, repo)?;
    let events = log.read_all()?;
    // Which items' latest status change was a run's claim.
    let mut by_run = BTreeMap::new();
    // Replaying first refuses a status change it can't read, so every one parses here. Only
    // this repository's items are ever taken, whatever the log holds of others.
    let items = replay_repo(&events, repo)?;
    let legacy = legacy_repo(&events, repo);
    for event in events
        .iter()
        .filter(|e| e.kind == EV_STATUS && event_repo(e, legacy) == repo)
    {
        if let Ok(change) = serde_json::from_value::<StatusChange>(event.data.clone()) {
            by_run.insert(change.number, change.run && change.status == Status::Active);
        }
    }
    for item in items.values() {
        let left =
            item.open && item.status == Status::Active && by_run.get(&item.number) == Some(&true);
        if !left || !wanted(item) {
            continue;
        }
        if let Some(run_lock) = home.try_run_lock(repo, item.number)? {
            return Ok(Some(RunClaim {
                item: item.clone(),
                lock: run_lock,
                resumed: true,
            }));
        }
    }
    let mut ordered: Vec<&WorkItem> = items.values().collect();
    ordered.sort_by_key(|item| item.priority);
    for item in ordered
        .into_iter()
        .filter(|item| is_ready(item) && wanted(item))
    {
        // A queued item a live run still holds (set back by hand mid-run) waits for that run.
        let Some(run_lock) = home.try_run_lock(repo, item.number)? else {
            continue;
        };
        log.append(
            &lock,
            EV_STATUS,
            json!({
                "repo": repo, "number": item.number, "status": Status::Active, "note": null,
                "run": true
            }),
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
    repo: &str,
    number: u64,
    status: Status,
    note: Option<&str>,
) -> Result<WorkItem> {
    let lock = home.lock()?;
    let log = home.events();
    pin_legacy(&lock, &log, repo)?;
    let mut item = replay_repo(&log.read_all()?, repo)?
        .remove(&number)
        .with_context(|| no_item(repo, number))?;
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
        json!({ "repo": repo, "number": number, "status": status, "note": note }),
    )?;
    item.status = status;
    item.note = note.map(str::to_string);
    Ok(item)
}

/// Import the open issues with the work label from the GitHub repository of the checkout at
/// `repo`. The label is the one `.paja/labels` names on the default branch.
pub fn sync(home: &Home, repo: &Path) -> Result<SyncReport> {
    sync_with(home, repo, None, None)
}

/// [`sync`] with the labels and queue rules already read (`paja run` reads them from the commit
/// it works on).
pub fn sync_with(
    home: &Home,
    repo: &Path,
    labels: Option<&Labels>,
    rules: Option<&QueueRules>,
) -> Result<SyncReport> {
    if !repo.is_dir() {
        bail!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            repo.display()
        );
    }
    // gh runs git in the checkout to find its GitHub repository.
    let ok = crate::ahjo::untampered(home, repo)?;
    let labels = match labels {
        Some(labels) => labels.clone(),
        None => Labels::of_default_branch(&ok)?,
    };
    let rules = match rules {
        Some(rules) => *rules,
        None => QueueRules::of_default_branch(&ok)?,
    };
    let name = name_of(&ok)?;
    let mut issues = fetch_issues(&ok, &labels.work)?;
    if rules.follow_ups {
        // An issue with both labels is a work item.
        let mut follow_ups = fetch_issues(&ok, &labels.follow_up)?;
        follow_ups.retain(|f| !issues.iter().any(|i| i.number == f.number));
        for issue in &mut follow_ups {
            issue.priority = Priority::FollowUp;
        }
        issues.extend(follow_ups);
    }
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
    apply_with(home, &name, &issues, &open_outside)
}

/// Bring the queue of `repo` in line with its current open issues. Separate from `sync` so it
/// can be tested without GitHub.
pub fn apply(home: &Home, repo: &str, open: &[Issue]) -> Result<SyncReport> {
    apply_with(home, repo, open, &BTreeSet::new())
}

/// [`apply`], where `open_outside` are the dependencies outside `open` that are still open.
pub fn apply_with(
    home: &Home,
    repo: &str,
    open: &[Issue],
    open_outside: &BTreeSet<u64>,
) -> Result<SyncReport> {
    let lock = home.lock()?;
    let log = home.events();
    pin_legacy(&lock, &log, repo)?;
    let events = log.read_all()?;
    let items = replay_repo(&events, repo)?;
    let recorded: BTreeSet<u64> = waiting_recorded(&events, legacy_repo(&events, repo))
        .into_iter()
        .filter(|(r, _)| r == repo)
        .map(|(_, number)| number)
        .collect();
    let mut report = SyncReport {
        repo: repo.to_string(),
        ..SyncReport::default()
    };
    // An issue as an event records it, with its repository.
    let issue_data = |issue: &Issue| -> Result<Value> {
        let mut data = serde_json::to_value(issue)?;
        data["repo"] = json!(repo);
        Ok(data)
    };
    for issue in open {
        match items.get(&issue.number) {
            None => {
                log.append(&lock, EV_IMPORTED, issue_data(issue)?)?;
                report.imported.push(issue.number);
            }
            Some(item) => {
                if !item.open {
                    log.append(
                        &lock,
                        EV_REOPENED,
                        json!({ "repo": repo, "number": issue.number }),
                    )?;
                    report.reopened.push(issue.number);
                }
                if (&item.title, &item.body, &item.url, item.priority)
                    != (&issue.title, &issue.body, &issue.url, issue.priority)
                {
                    log.append(&lock, EV_CHANGED, issue_data(issue)?)?;
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
                json!({ "repo": repo, "number": issue.number, "on": on }),
            )?;
        }
    }
    for item in items.values() {
        let still_open = open.iter().any(|issue| issue.number == item.number);
        if item.open && !still_open {
            log.append(
                &lock,
                EV_CLOSED,
                json!({ "repo": repo, "number": item.number }),
            )?;
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
    let name = name_of(&ok)?;
    let item = {
        let lock = home.lock()?;
        let log = home.events();
        pin_legacy(&lock, &log, &name)?;
        let mut item = replay_repo(&log.read_all()?, &name)?
            .remove(&number)
            .with_context(|| no_item(&name, number))?;
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
            json!({
                "repo": name, "number": number, "question": answer.question,
                "answer": answer.answer
            }),
        )?;
        log.append(
            &lock,
            EV_STATUS,
            json!({ "repo": name, "number": number, "status": Status::Queued, "note": null }),
        )?;
        item.answers.push(answer);
        item.status = Status::Queued;
        item.note = None;
        item
    };
    // The item no longer waits on the person. An issue escalated before Paja labelled
    // escalations has no label to remove, so a failure here only says so.
    let needs_answer = Labels::of_default_branch(&ok)
        .map(|labels| labels.needs_answer)
        .unwrap_or_else(|_| Labels::default().needs_answer);
    if let Err(err) = gh(
        &ok,
        &[
            "issue",
            "edit",
            &number.to_string(),
            "--remove-label",
            &needs_answer,
        ],
    ) {
        eprintln!("paja: couldn't remove the `{needs_answer}` label from #{number}: {err:#}");
    }
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

/// Put the repository's `needs-answer` label on escalated item `number`'s issue, creating the
/// label first when the repository doesn't have it yet. Returns the label.
pub fn mark_needs_answer(home: &Home, repo: &Path, number: u64) -> Result<String> {
    let ok = crate::ahjo::untampered(home, repo)?;
    let label = Labels::of_default_branch(&ok)?.needs_answer;
    let add = |ok: &Untampered| {
        gh(
            ok,
            &["issue", "edit", &number.to_string(), "--add-label", &label],
        )
    };
    if add(&ok).is_err() {
        gh(
            &ok,
            &[
                "label",
                "create",
                &label,
                "--color",
                "d93f0b",
                "--description",
                "Paja is waiting for an answer: paja work answer <number> \"<answer>\"",
            ],
        )?;
        add(&ok)?;
    }
    Ok(label)
}

/// The local checkout of `repo` (`owner/name`) that Paja last worked in: the repository an
/// environment for one of its items was made from, newest first, when it still exists. None
/// when no item of `repo` has had an environment yet. For a front end that starts outside any
/// checkout, such as Paja Desk, so it can run the core's commands where `gh` finds `repo`.
pub fn known_checkout(home: &Home, repo: &str) -> Result<Option<std::path::PathBuf>> {
    let events = home.events().read_all()?;
    let prefix = env_name(repo, 0);
    let prefix = prefix.strip_suffix('0').unwrap_or(&prefix);
    Ok(events
        .iter()
        .rev()
        .filter(|e| e.kind == crate::ahjo::EV_CREATED)
        .filter(|e| {
            e.data["name"].as_str().is_some_and(|name| {
                name.strip_prefix(prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
        })
        .filter_map(|e| e.data["repo"].as_str().map(std::path::PathBuf::from))
        .find(|path| path.is_dir()))
}

/// A GitHub issue filed for Jono.
#[derive(Debug, Clone, Serialize)]
pub struct Filed {
    pub url: String,
}

/// File a new GitHub issue with the repository's work label, for example a spec a person
/// approved. The next sync imports it as queued work.
pub fn file(home: &Home, repo: &Path, title: &str, body: &str) -> Result<Filed> {
    if title.trim().is_empty() {
        bail!("the issue needs a title; pass --title");
    }
    let ok = crate::ahjo::untampered(home, repo)?;
    let label = Labels::of_default_branch(&ok)?.work;
    let url = gh(
        &ok,
        &[
            "issue", "create", "--label", &label, "--title", title, "--body", body,
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

pub(crate) fn gh(ok: &Untampered, args: &[&str]) -> Result<String> {
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

fn fetch_issues(ok: &Untampered, label: &str) -> Result<Vec<Issue>> {
    let repo = ok.path();
    let gh = gh_program();
    let output = gh_output(
        ok,
        &[
            "issue",
            "list",
            "--label",
            label,
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
            "{FETCH_LIMIT} or more open issues are labelled `{label}`, more than one sync reads; \
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
            priority: Priority::Work,
        }
    }

    #[test]
    fn labels_default_to_neutral_names_and_read_both_keys() {
        assert_eq!(Labels::parse("").unwrap(), Labels::default());
        assert_eq!(Labels::default().work, "factory");
        assert_eq!(Labels::default().follow_up, "factory-follow-up");
        let both = Labels::parse("# mine\nwork game\nfollow-up game ideas\n").unwrap();
        assert_eq!(
            (both.work.as_str(), both.follow_up.as_str()),
            ("game", "game ideas")
        );
        let one = Labels::parse("follow-up later\n").unwrap();
        assert_eq!(
            (one.work.as_str(), one.follow_up.as_str()),
            ("factory", "later")
        );
        for bad in [
            "work",
            "work -x",
            "work a,b",
            "nope x",
            "work a\nwork b",
            "work x\nfollow-up x",
            "needs-answer factory",
            "follow-up y\nneeds-answer y",
            "needs-answer a\nneeds-answer b",
        ] {
            assert!(Labels::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Labels::default().needs_answer, "factory-needs-answer");
        let waiting = Labels::parse("needs-answer waiting on you\n").unwrap();
        assert_eq!(waiting.needs_answer, "waiting on you");
    }

    #[test]
    fn known_checkout_is_the_newest_existing_one_of_that_repository() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let (old, new, gone, other) = ("old", "new", "gone", "other");
        for name in [old, new, other] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        let lock = home.lock().unwrap();
        let log = home.events();
        let env = |repo: &str, number: u64, path: &str| {
            json!({
                "name": env_name(repo, number),
                "repo": dir.path().join(path),
                "path": dir.path().join("env"),
                "branch": "b",
                "base_commit": "0",
                "port_base": 41000,
                "port_count": 10,
                "created_ms": 0,
            })
        };
        log.append(&lock, crate::ahjo::EV_CREATED, env(R, 1, old))
            .unwrap();
        log.append(&lock, crate::ahjo::EV_CREATED, env(R, 12, new))
            .unwrap();
        log.append(&lock, crate::ahjo::EV_CREATED, env(R, 2, gone))
            .unwrap();
        log.append(
            &lock,
            crate::ahjo::EV_CREATED,
            env("acme/application", 3, other),
        )
        .unwrap();
        drop(lock);
        // The newest of R's whose folder still exists; another repository's never counts.
        assert_eq!(
            known_checkout(&home, R).unwrap(),
            Some(dir.path().join(new))
        );
        assert_eq!(known_checkout(&home, "acme/none").unwrap(), None);
    }

    // The queue of one repository, which most tests are about.
    const R: &str = "acme/app";

    fn apply(home: &Home, open: &[Issue]) -> Result<SyncReport> {
        super::apply(home, R, open)
    }

    fn apply_with(home: &Home, open: &[Issue], outside: &BTreeSet<u64>) -> Result<SyncReport> {
        super::apply_with(home, R, open, outside)
    }

    fn get(home: &Home, number: u64) -> Result<WorkItem> {
        super::get(home, R, number)
    }

    fn list(home: &Home) -> Result<Vec<WorkItem>> {
        super::list(home, R)
    }

    fn next(home: &Home) -> Result<Option<WorkItem>> {
        super::next(home, R)
    }

    fn claim(home: &Home) -> Result<Option<WorkItem>> {
        super::claim(home, R)
    }

    fn claim_for_run(home: &Home) -> Result<Option<RunClaim>> {
        super::claim_for_run(home, R)
    }

    fn waiting(home: &Home) -> Result<Option<String>> {
        super::waiting(home, R)
    }

    fn set_status(
        home: &Home,
        number: u64,
        status: Status,
        note: Option<&str>,
    ) -> Result<WorkItem> {
        super::set_status(home, R, number, status, note)
    }

    #[test]
    fn two_repositories_keep_their_own_items_with_the_same_number() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let (a, b) = ("acme/app", "acme/site");
        super::apply(&home, a, &[issue(1, "A one")]).unwrap();
        super::apply(&home, b, &[issue(1, "B one")]).unwrap();
        super::set_status(&home, a, 1, Status::Escalated, Some("which API?")).unwrap();
        assert_eq!(super::get(&home, b, 1).unwrap().status, Status::Queued);
        // A sync of one repository doesn't close or change the other's items.
        super::apply(&home, b, &[]).unwrap();
        let item = super::get(&home, a, 1).unwrap();
        assert_eq!((item.open, item.title.as_str()), (true, "A one"));
        assert!(!super::get(&home, b, 1).unwrap().open);
        // A claim and a run's claim take only the repository's own items.
        assert!(super::claim(&home, a).unwrap().is_none());
        super::apply(&home, b, &[issue(1, "B one")]).unwrap();
        assert!(super::claim_for_run(&home, a).unwrap().is_none());
        let claimed = super::claim_for_run(&home, b).unwrap().unwrap();
        assert_eq!((claimed.item.repo.as_str(), claimed.item.number), (b, 1));
        assert_eq!(super::get(&home, a, 1).unwrap().status, Status::Escalated);
        let all = list_all(&home, a).unwrap();
        let keys: Vec<(&str, u64)> = all.iter().map(|i| (i.repo.as_str(), i.number)).collect();
        assert_eq!(keys, [(a, 1), (b, 1)]);
    }

    #[test]
    fn events_from_before_repositories_belong_to_the_first_sync_or_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let lock = home.lock().unwrap();
        let log = home.events();
        let old = issue(4, "Old");
        log.append(&lock, EV_IMPORTED, serde_json::to_value(&old).unwrap())
            .unwrap();
        log.append(
            &lock,
            EV_STATUS,
            json!({ "number": 4, "status": "escalated", "note": "Q?" }),
        )
        .unwrap();
        log.append(&lock, EV_ANSWERED, json!({ "number": 4, "answer": "A" }))
            .unwrap();
        drop(lock);
        let before = log.read_all().unwrap();
        // With no repository named yet, they belong to the checkout the command runs in.
        for repo in ["acme/app", "acme/site"] {
            let items = replay(&before, repo).unwrap();
            let item = &items[&(repo.to_string(), 4)];
            assert_eq!((item.status, item.answers.len()), (Status::Escalated, 1));
            assert_eq!(items.len(), 1);
        }
        // The first write pins them to the repository that wrote it, and a later one from
        // another checkout leaves them there.
        super::set_status(&home, "acme/app", 4, Status::Landed, None).unwrap();
        super::apply(&home, "acme/site", &[issue(4, "Other")]).unwrap();
        let log = log.read_all().unwrap();
        assert_eq!(&log[..before.len()], &before[..], "nothing was rewritten");
        let items = replay(&log, "acme/site").unwrap();
        assert_eq!(items[&("acme/app".to_string(), 4)].title, "Old");
        assert_eq!(items[&("acme/app".to_string(), 4)].status, Status::Landed);
        assert_eq!(items[&("acme/site".to_string(), 4)].title, "Other");
        assert_eq!(items[&("acme/site".to_string(), 4)].status, Status::Queued);
    }

    #[test]
    fn environment_names_tell_repositories_apart_and_fit_ahjo() {
        let a = env_name("acme/app", 5);
        let b = env_name("other/app", 5);
        assert_ne!(a, b);
        assert!(a.starts_with("app-") && a.ends_with("-work-5"), "{a}");
        for name in [
            a,
            b,
            env_name("Some-Owner/A_Very.Long-Repository-Name", u64::MAX),
            env_name("o/___", 1),
        ] {
            assert!(name.len() <= 40, "{name}");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                    && !name.starts_with('-'),
                "{name}"
            );
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
        assert_eq!(usage.unreadable_events.len(), 2, "{usage:?}");
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
