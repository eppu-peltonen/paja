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
use crate::mestari::{StepUsage, EV_ARBITRATED, EV_ROUND};

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
    /// The label a claimed item carries on GitHub: the work label and `-claimed` (`paja-claimed`
    /// for `paja`).
    pub fn claimed(&self) -> String {
        format!("{}-claimed", self.work)
    }

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
/// Set on a `work.closed` event when the item is a follow-up that a sync no longer listed
/// because `.paja/queue` says `follow-ups = off`, so a landed one stays landed when they come
/// back on.
const FOLLOW_UPS_OFF: &str = "follow_ups_off";
/// The mark on a `work.reopened` of a follow-up hidden by `follow-ups = off` that GitHub
/// reopened while it was hidden (a revert, say): it is a real reopen, so a landed one is queued.
const REOPENED_ON_GITHUB: &str = "reopened_on_github";
pub const EV_STATUS: &str = "work.status";
/// A person answered an escalated item's question.
pub const EV_ANSWERED: &str = "work.answered";
/// A sync found which of an item's dependencies are still open: `{"number", "on": [...]}`.
pub const EV_WAITING: &str = "work.waiting";
/// A repository's first appearance in a log that holds events from before work items carried
/// one: `{"repo"}`. It pins those older events to that repository (see [`legacy_repo`]).
pub const EV_SYNCED: &str = "work.synced";
/// A sync ran in a checkout of the repository: `{"repo", "path"}`, recorded when the path
/// differs from the last one recorded, so Paja Desk knows where to run `gh` and `paja run`.
pub const EV_CHECKOUT: &str = "work.checkout";
/// The id this home's runner claims items under, created once: `{"id"}`.
pub const EV_RUNNER: &str = "runner.created";
/// What GitHub says about an item's claim, recorded when it changes: `{"repo", "number",
/// "runner", "stale_after"}`, with `runner` null once the claim is gone.
pub const EV_CLAIM: &str = "work.claim";

/// How long a claim lasts without being renewed. A run renews it at each round.
pub const CLAIM_SECS: u64 = 6 * 60 * 60;

/// Who holds an item on GitHub: the claimed label, and the runner in its first live claim comment
/// (else the latest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// The runner id in the first live claim comment (else the latest); `unknown` when the label
    /// has no comment.
    pub runner: String,
    /// The UTC time after which another runner may take the item. None when the claim can't go
    /// stale: the item is escalated and waits on a person, or the label has no comment.
    #[serde(default)]
    pub stale_after: Option<String>,
}

impl Claim {
    fn is_stale(&self, now: u64) -> bool {
        self.stale_after
            .as_deref()
            .and_then(parse_utc)
            .is_some_and(|t| t <= now)
    }
}

/// The runner id of the first `runner.created` event, if one was recorded.
fn runner_of(events: &[Event]) -> Option<&str> {
    events
        .iter()
        .find(|e| e.kind == EV_RUNNER)
        .and_then(|e| e.data["id"].as_str())
}

/// The runner holding `item` when it is not the runner of `events` and its claim is still live
/// at `now`: such an item is skipped.
pub fn held_by_other<'a>(events: &[Event], item: &'a WorkItem, now: u64) -> Option<&'a str> {
    let claim = item.claim.as_ref()?;
    (Some(claim.runner.as_str()) != runner_of(events) && !claim.is_stale(now))
        .then_some(claim.runner.as_str())
}

/// The seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    crate::events::now_ms() / 1000
}

/// `secs` as `2026-01-31T09:05:00Z`.
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Days since 1970-01-01 to a calendar date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The seconds since the Unix epoch of a time written as `2026-01-31T09:05:00Z`.
pub(crate) fn parse_utc(text: &str) -> Option<u64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-');
    let mut t = time.split(':');
    let (year, month, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let (h, m, s): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if d.next().is_some()
        || t.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&h)
        || !(0..60).contains(&m)
        || !(0..61).contains(&s)
        || year < 1970
    {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

/// The text of a claim comment: `Claimed by runner <id> at <UTC time>; stale after <time>`.
fn claim_text(runner: &str, now: u64) -> String {
    format!(
        "Claimed by runner {runner} at {}; stale after {}",
        format_utc(now),
        format_utc(now + CLAIM_SECS)
    )
}

/// A claim comment read back: the runner and the time it goes stale.
struct ClaimComment {
    runner: String,
    stale_after: String,
    /// The comment's id on GitHub, from its URL.
    id: Option<u64>,
}

impl ClaimComment {
    fn is_stale(&self, now: u64) -> bool {
        parse_utc(&self.stale_after).is_none_or(|t| t <= now)
    }
}

/// Read a comment as a claim, or None when it isn't one.
fn parse_claim_comment(body: &str, url: &str) -> Option<ClaimComment> {
    let line = body.lines().next()?.trim();
    let rest = line.strip_prefix("Claimed by runner ")?;
    let (runner, rest) = rest.split_once(" at ")?;
    let (at, stale_after) = rest.split_once("; stale after ")?;
    parse_utc(at)?;
    parse_utc(stale_after)?;
    if runner.is_empty()
        || !runner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return None;
    }
    let id = url
        .rsplit_once("issuecomment-")
        .and_then(|(_, id)| id.parse().ok());
    Some(ClaimComment {
        runner: runner.to_string(),
        stale_after: stale_after.trim().to_string(),
        id,
    })
}

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
    /// Who holds the item on GitHub, as the last sync or claim saw it. A run takes an item held
    /// by another runner only once the claim is stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
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
    env_name_for(repo, &format!("work-{number}"))
}

/// An Ahjo environment name of `repo` for `what` (`work-5`, `triage`): the repository's name,
/// a short hash of `owner/name`, and `what`; at most 40 characters as long as `what` leaves
/// room. Two clones of one repository get the same names.
pub fn env_name_for(repo: &str, what: &str) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo).to_ascii_lowercase();
    let name: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    // What is left of the 40 after the hash, `-<what>` and the dash after the name.
    let room = 40usize.saturating_sub(6 + 1 + what.len() + 1);
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
    format!("{head}{:06x}-{what}", hash & 0xff_ffff)
}

/// A person's answer to a question Paja asked about an item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The question, as the escalation asked it, when it was known.
    #[serde(default)]
    pub question: Option<String>,
    pub answer: String,
    /// Who answered when it wasn't a person: `arbiter` for Mestari's arbiter agent. Absent for
    /// a person's answer, and in older logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// The `by` of an answer Mestari's arbiter agent gave.
pub const BY_ARBITER: &str = "arbiter";

impl Answer {
    /// Whether the arbiter agent gave this answer, not a person.
    pub fn by_arbiter(&self) -> bool {
        self.by.as_deref() == Some(BY_ARBITER)
    }
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
struct ClaimSeen {
    number: u64,
    #[serde(default)]
    runner: Option<String>,
    #[serde(default)]
    stale_after: Option<String>,
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
/// checkout can't move them. Every writer calls this under the home lock. Returns the log as it
/// is after, so the caller doesn't read it a second time.
fn pin_legacy(
    lock: &crate::home::HomeLock,
    log: &crate::events::EventLog,
    repo: &str,
) -> Result<Vec<Event>> {
    let events = log.read_all()?;
    let unkeyed = events
        .iter()
        .any(|e| is_item_event(e) && e.data["repo"].as_str().is_none());
    let pinned = events
        .iter()
        .any(|e| e.kind.starts_with("work.") && e.data["repo"].as_str().is_some());
    if unkeyed && !pinned {
        log.append(lock, EV_SYNCED, json!({ "repo": repo }))?;
        return log.read_all();
    }
    Ok(events)
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
    Ok(replay_with_hidden(events, fallback)?.0)
}

/// [`replay`], and the closed items whose last close was a follow-up hidden by
/// `follow-ups = off` rather than closed on GitHub.
fn replay_with_hidden(
    events: &[Event],
    fallback: &str,
) -> Result<(BTreeMap<Key, WorkItem>, BTreeSet<Key>)> {
    let mut items: BTreeMap<Key, WorkItem> = BTreeMap::new();
    // Follow-ups whose last close came from `follow-ups = off`, not from GitHub.
    let mut hidden: BTreeSet<Key> = BTreeSet::new();
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
                        claim: None,
                    },
                );
            }
            EV_CLAIM => {
                let seen: ClaimSeen =
                    serde_json::from_value(event.data.clone()).with_context(|| bad(event))?;
                if let Some(item) = items.get_mut(&key(seen.number)) {
                    item.claim = seen.runner.map(|runner| Claim {
                        runner,
                        stale_after: seen.stale_after,
                    });
                }
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
                    // A landed issue opened again (say after a revert) needs work again, but
                    // not a follow-up that only comes back because follow-ups are on again.
                    let back_from_rules = if item.open {
                        hidden.remove(&key(n.number)) && event.data[REOPENED_ON_GITHUB] != true
                    } else {
                        if event.data[FOLLOW_UPS_OFF] == true {
                            hidden.insert(key(n.number));
                        } else {
                            hidden.remove(&key(n.number));
                        }
                        false
                    };
                    if item.open && item.status == Status::Landed && !back_from_rules {
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
            EV_ROUND | EV_ARBITRATED => {
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
    Ok((items, hidden))
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
    repo_name_within(home, path, None)
}

/// [`repo_name`], with `gh` stopped once it has run for `limit`.
pub(crate) fn repo_name_within(
    home: &Home,
    path: &Path,
    limit: Option<std::time::Duration>,
) -> Result<String> {
    if !path.is_dir() {
        bail!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            path.display()
        );
    }
    match crate::ahjo::untampered(home, path) {
        Ok(ok) => name_of(&ok, limit),
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

fn name_of(ok: &Untampered, limit: Option<std::time::Duration>) -> Result<String> {
    let out = gh_within(ok, &["repo", "view", "--json", "nameWithOwner"], limit)?;
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
    let events = home.events().read_all()?;
    let item = replay_repo(&events, repo)?
        .remove(&number)
        .with_context(|| no_item(repo, number))?;
    let mut shown = serde_json::to_value(&item)?;
    if item.status == Status::Active {
        shown["run"] = serde_json::to_value(crate::mestari::live::live_of(home, &events, &item)?)?;
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
    let events = pin_legacy(&lock, &log, repo)?;
    let mut items: Vec<WorkItem> = replay_repo(&events, repo)?.into_values().collect();
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
    claim_for_run_item(home, repo, None, true)
}

/// [`claim_for_run`], taking only item `only` when one is named (`paja run --item`). With
/// `new` false only an item to resume is taken: a queued one stays queued, with no status event.
pub fn claim_for_run_item(
    home: &Home,
    repo: &str,
    only: Option<u64>,
    new: bool,
) -> Result<Option<RunClaim>> {
    let wanted = |item: &WorkItem| only.is_none_or(|n| n == item.number);
    let lock = home.lock()?;
    let log = home.events();
    let events = pin_legacy(&lock, &log, repo)?;
    let (left, ready) = run_candidates(&events, repo)?;
    for item in left.into_iter().filter(wanted) {
        if let Some(run_lock) = home.try_run_lock(repo, item.number)? {
            return Ok(Some(RunClaim {
                item,
                lock: run_lock,
                resumed: true,
            }));
        }
    }
    for item in ready.into_iter().filter(wanted).filter(|_| new) {
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
        let mut item = item;
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

/// This home's runner id, created and recorded on first use. It is random and names no host or
/// user, because claims are written on GitHub, where the repository may be public.
pub fn runner_id(home: &Home) -> Result<String> {
    let lock = home.lock()?;
    runner_id_locked(&lock, &home.events())
}

fn runner_id_locked(lock: &crate::home::HomeLock, log: &crate::events::EventLog) -> Result<String> {
    if let Some(id) = runner_of(&log.read_all()?) {
        return Ok(id.to_string());
    }
    use std::hash::{BuildHasher, Hasher, RandomState};
    let id: String = (0..2)
        .map(|_| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(crate::events::now_ms());
            hasher.write_u32(std::process::id());
            format!("{:08x}", hasher.finish() as u32)
        })
        .collect();
    log.append(lock, EV_RUNNER, json!({ "id": id }))?;
    Ok(id)
}

/// [`claim_for_run_item`] for runners that share the queue through GitHub: before an item is
/// taken, its claim is written on the issue (the claimed label and a claim comment), and the
/// issue is read again to see that no other runner's claim came first. An item that lost the
/// race is left to the winner and the next one is tried. When GitHub can't be reached nothing is
/// taken, and the error says why. `checkout` is where `gh` runs. With `new` false only an item
/// to resume is taken, and nothing is claimed on GitHub.
pub fn claim_for_run_on(
    home: &Home,
    checkout: &Path,
    repo: &str,
    only: Option<u64>,
    new: bool,
) -> Result<Option<RunClaim>> {
    let wanted = |item: &WorkItem| only.is_none_or(|n| n == item.number);
    let ok = crate::ahjo::untampered(home, checkout)?;
    let labels = Labels::of_default_branch(&ok)?;
    let (me, ready) = {
        let lock = home.lock()?;
        let log = home.events();
        let events = pin_legacy(&lock, &log, repo)?;
        let me = runner_id_locked(&lock, &log)?;
        let (left, ready) = run_candidates(&events, repo)?;
        // An item this runner left active keeps its claim, which each round renews.
        for item in left.into_iter().filter(wanted) {
            if let Some(run_lock) = home.try_run_lock(repo, item.number)? {
                return Ok(Some(RunClaim {
                    item,
                    lock: run_lock,
                    resumed: true,
                }));
            }
        }
        (me, ready)
    };
    for item in ready.into_iter().filter(wanted).filter(|_| new) {
        let run_lock = {
            let _lock = home.lock()?;
            home.try_run_lock(repo, item.number)?
        };
        // A queued item a live run still holds (set back by hand mid-run) waits for that run.
        let Some(run_lock) = run_lock else {
            continue;
        };
        let race = claim_on_github(&ok, &labels, item.number, &me)?;
        let lock = home.lock()?;
        let log = home.events();
        match race {
            Race::Won => {}
            Race::LostTo(winner) => {
                log.append(
                    &lock,
                    EV_CLAIM,
                    claim_event(repo, item.number, Some(&winner)),
                )?;
                continue;
            }
            Race::Gone => continue,
        }
        let mine = Claim {
            runner: me.clone(),
            stale_after: Some(format_utc(now_secs() + CLAIM_SECS)),
        };
        log.append(&lock, EV_CLAIM, claim_event(repo, item.number, Some(&mine)))?;
        log.append(
            &lock,
            EV_STATUS,
            json!({
                "repo": repo, "number": item.number, "status": Status::Active, "note": null,
                "run": true
            }),
        )?;
        let mut item = item;
        item.status = Status::Active;
        item.note = None;
        item.claim = Some(mine);
        return Ok(Some(RunClaim {
            item,
            lock: run_lock,
            resumed: false,
        }));
    }
    Ok(None)
}

fn claim_event(repo: &str, number: u64, claim: Option<&Claim>) -> Value {
    json!({
        "repo": repo, "number": number,
        "runner": claim.map(|c| c.runner.as_str()),
        "stale_after": claim.and_then(|c| c.stale_after.as_deref()),
    })
}

/// An issue as `gh issue view --json labels,comments` prints it.
#[derive(Deserialize)]
struct View {
    #[serde(default)]
    labels: Vec<LabelName>,
    #[serde(default)]
    comments: Vec<ViewComment>,
}

#[derive(Deserialize)]
struct LabelName {
    name: String,
}

#[derive(Deserialize)]
struct ViewComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    url: String,
}

impl View {
    fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l.name == label)
    }

    /// The claim comments, oldest first.
    fn claims(&self) -> Vec<ClaimComment> {
        self.comments
            .iter()
            .filter_map(|c| parse_claim_comment(&c.body, &c.url))
            .collect()
    }
}

fn view_issue(ok: &Untampered, number: u64) -> Result<View> {
    let out = gh(
        ok,
        &[
            "issue",
            "view",
            &number.to_string(),
            "--json",
            "labels,comments",
        ],
    )?;
    serde_json::from_str(&out).with_context(|| {
        format!(
            "`{} issue view {number}` printed something that is not an issue's labels and \
             comments; check `gh auth status`",
            gh_program()
        )
    })
}

/// Who holds an issue, from its labels and comments, when the issue has the claimed label: the
/// runner in the first live claim comment, the one that won the race (a loser whose comment
/// couldn't be deleted is later), else in the latest claim comment. An escalated item (it
/// carries `needs_answer`) stays claimed however old its claim is.
fn holder(view: &View, labels: &Labels) -> Option<Claim> {
    if !view.has_label(&labels.claimed()) {
        return None;
    }
    let escalated = view.has_label(&labels.needs_answer);
    let claims = view.claims();
    let chosen = race_winner(&claims, now_secs()).or(claims.last());
    Some(match chosen {
        Some(c) => held(c, escalated),
        None => Claim {
            runner: "unknown".into(),
            stale_after: None,
        },
    })
}

/// The claim that won the race for an issue: its first live claim comment. Every reader of a
/// race (the claim check, `holder`, freeing) goes by this one rule.
fn race_winner(claims: &[ClaimComment], now: u64) -> Option<&ClaimComment> {
    claims.iter().find(|c| !c.is_stale(now))
}

/// The claim a comment records: its runner and stale time, with no stale time for an escalated
/// item, which waits on a person and so can't go stale.
fn held(c: &ClaimComment, escalated: bool) -> Claim {
    Claim {
        runner: c.runner.clone(),
        stale_after: (!escalated).then(|| c.stale_after.clone()),
    }
}

/// How a claim race on GitHub came out.
#[derive(Debug, PartialEq, Eq)]
enum Race {
    Won,
    /// Another runner's live claim came first: the holder, recorded as such.
    LostTo(Claim),
    /// This runner's own comment was gone when it read the issue back: a runner giving the
    /// item back deleted it as a later claim. Nobody holds the item by this runner's reckoning.
    Gone,
}

/// Read a race back: `own` is the id of the comment this runner just posted, when `gh` printed
/// it. The runner won only when its own live comment is still there and is the race's winner.
fn race_result(claims: &[ClaimComment], me: &str, own: Option<u64>, now: u64) -> Race {
    if let Some(w) = race_winner(claims, now).filter(|w| w.runner != me) {
        return Race::LostTo(held(w, false));
    }
    let is_own = |c: &ClaimComment| match own {
        Some(own) => c.id == Some(own),
        None => c.runner == me,
    };
    if claims.iter().any(|c| is_own(c) && !c.is_stale(now)) {
        Race::Won
    } else {
        Race::Gone
    }
}

/// Write this runner's claim on issue `number` and check that it won (see `race_result`). On a
/// loss to another runner's live claim, which comes first, this runner's comment is deleted.
/// The claimed label stays then: it is shared, and the winner's live claim is what it now
/// marks, so taking it off would make the item look free until the winner's next renewal. When
/// this runner's own comment is gone, or a step fails, the claim is taken back (see
/// `take_back`); a failure's error says the item was not taken.
fn claim_on_github(ok: &Untampered, labels: &Labels, number: u64, me: &str) -> Result<Race> {
    let claimed = labels.claimed();
    add_label(
        ok,
        number,
        &claimed,
        "fbca04",
        "A Paja runner holds this item; the claim comment says who and until when",
    )?;
    let now = now_secs();
    let posted = gh(
        ok,
        &[
            "issue",
            "comment",
            &number.to_string(),
            "--body",
            &claim_text(me, now),
        ],
    );
    let not_taken = |err: anyhow::Error| {
        err.context(format!(
            "claiming #{number} on GitHub failed, so it was not taken"
        ))
    };
    let own = match posted {
        Ok(out) => out
            .lines()
            .last()
            .unwrap_or_default()
            .trim()
            .rsplit_once("issuecomment-")
            .and_then(|(_, id)| id.parse::<u64>().ok()),
        Err(err) => {
            take_back(ok, &claimed, number, me, None);
            return Err(not_taken(err));
        }
    };
    let view = match view_issue(ok, number) {
        Ok(view) => view,
        Err(err) => {
            take_back(ok, &claimed, number, me, own);
            return Err(not_taken(err));
        }
    };
    let race = race_result(&view.claims(), me, own, now);
    match (&race, own) {
        (Race::LostTo(_), Some(id)) => {
            let _ = delete_comment(ok, id);
        }
        // The runner that deleted this one's comment also took the label off when nobody
        // else held the item; make sure, so the item isn't left labelled with no claim.
        (Race::Gone, _) => take_back(ok, &claimed, number, me, None),
        _ => {}
    }
    Ok(race)
}

fn delete_comment(ok: &Untampered, id: u64) -> Result<()> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
    gh(ok, &["api", "-X", "DELETE", &path]).map(drop)
}

/// Undo a claim that failed part way: delete this runner's comment (`own`, when it was posted),
/// then take the shared claimed label off unless another runner's live claim is on the issue,
/// whose label it is (see `label_is_another_runners`). When the issue can't be read the label
/// comes off, so a failure never leaves an item looking claimed by nobody in particular. Best
/// effort: the caller reports the failure that brought it here.
fn take_back(ok: &Untampered, claimed: &str, number: u64, me: &str, own: Option<u64>) {
    if let Some(id) = own {
        let _ = delete_comment(ok, id);
    }
    let now = now_secs();
    let other = view_issue(ok, number)
        .is_ok_and(|view| label_is_another_runners(&view.claims(), me, own, now));
    if !other {
        let _ = remove_label(ok, number, claimed);
    }
}

/// Whether another runner's live claim holds the claimed label, so taking back this runner's
/// claim leaves it on. With `own` (this runner's comment id) known, only a claim posted before
/// it counts: a later one lost the race to this runner's comment and is about to delete itself.
/// A claim whose id can't be read counts, which keeps the label.
fn label_is_another_runners(claims: &[ClaimComment], me: &str, own: Option<u64>, now: u64) -> bool {
    claims.iter().any(|c| {
        c.runner != me && !c.is_stale(now) && own.is_none_or(|own| c.id.is_none_or(|id| id < own))
    })
}

fn add_label(
    ok: &Untampered,
    number: u64,
    label: &str,
    color: &str,
    description: &str,
) -> Result<()> {
    let add = |ok: &Untampered| {
        gh(
            ok,
            &["issue", "edit", &number.to_string(), "--add-label", label],
        )
    };
    match add(ok) {
        Ok(_) => Ok(()),
        // Only a label the repository lacks is created; any other failure (the network, a
        // deleted issue, no permission) is the error, not a reason to make a label.
        Err(err) if label_missing(&err) => {
            gh(
                ok,
                &[
                    "label",
                    "create",
                    label,
                    "--color",
                    color,
                    "--description",
                    description,
                ],
            )?;
            add(ok).map(drop)
        }
        Err(err) => Err(err),
    }
}

/// Whether `gh issue edit --add-label` failed because the repository has no such label: gh
/// says `could not add label: '<label>' not found`. Matched by that phrase, not by the words
/// alone, so a checkout path or another error that happens to say "label" or "not found"
/// isn't taken for it.
fn label_missing(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}").to_lowercase();
    text.split("could not add label: ").skip(1).any(|rest| {
        rest.lines()
            .next()
            .is_some_and(|l| l.contains("' not found"))
    })
}

fn remove_label(ok: &Untampered, number: u64, label: &str) -> Result<()> {
    gh(
        ok,
        &[
            "issue",
            "edit",
            &number.to_string(),
            "--remove-label",
            label,
        ],
    )
    .map(drop)
}

/// Renew this runner's claim on `number`: move its live claim comment's times on (a stale one is
/// posted again, then deleted), and put the claimed label back if it was taken off. A claim that
/// went stale while another runner claimed the item is not renewed: that runner holds it, and
/// the error says so. A run calls this at each round; the caller reports a failure and goes on,
/// since the claim only matters to other runners.
pub fn renew_claim(home: &Home, checkout: &Path, number: u64) -> Result<()> {
    let me = runner_id(home)?;
    let ok = crate::ahjo::untampered(home, checkout)?;
    let labels = Labels::of_default_branch(&ok)?;
    let claimed = labels.claimed();
    let view = view_issue(&ok, number)?;
    let now = now_secs();
    let text = claim_text(&me, now);
    let mine = view.claims().into_iter().rev().find(|c| c.runner == me);
    // A stale claim is posted again rather than edited: edited, it would keep its old place
    // in comment order and win the race against a claim posted while it was stale.
    let stale = mine.as_ref().filter(|c| c.is_stale(now)).and_then(|c| c.id);
    let live_mine = mine.as_ref().is_some_and(|c| !c.is_stale(now));
    let claims = view.claims();
    if let Some(other) = race_winner(&claims, now).filter(|w| w.runner != me && !live_mine) {
        bail!(
            "#{number}'s claim lapsed and runner {} claimed it on GitHub since, so it holds the \
             item now; this run goes on, and its work may duplicate that runner's. Stop one of \
             the two runs",
            other.runner
        );
    }
    match mine.filter(|c| !c.is_stale(now)).and_then(|c| c.id) {
        Some(id) => {
            let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
            gh(
                &ok,
                &["api", "-X", "PATCH", &path, "-f", &format!("body={text}")],
            )?;
        }
        None => {
            gh(
                &ok,
                &["issue", "comment", &number.to_string(), "--body", &text],
            )?;
            // The stale one goes once the new one is up; left, it is only noise.
            if let Some(id) = stale {
                let _ = delete_comment(&ok, id);
            }
        }
    }
    if !view.has_label(&claimed) {
        add_label(
            &ok,
            number,
            &claimed,
            "fbca04",
            "A Paja runner holds this item",
        )?;
    }
    let mine = Claim {
        runner: me,
        stale_after: Some(format_utc(now + CLAIM_SECS)),
    };
    let name = name_of(&ok, None)?;
    let lock = home.lock()?;
    home.events()
        .append(&lock, EV_CLAIM, claim_event(&name, number, Some(&mine)))?;
    Ok(())
}

/// Whose live claim comments freeing an item deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Freeing {
    /// Every runner's: the person put the item back (`paja work set N queued`), which overrides
    /// whichever runner held it.
    Every,
    /// Only this runner's: a run gives back an item it claimed but won't build (the person
    /// declined, or a gate put it back). Another runner's live claim stands, label included.
    Own,
}

/// Take the claimed label off `number`'s issue and try to delete every live claim comment, so
/// any runner may take the item: the person put it back. A failure is reported on stderr and
/// nothing more, since the item's status is already recorded.
pub fn release_claim(home: &Home, checkout: &Path, number: u64) {
    free_claim(home, checkout, number, Freeing::Every);
}

/// Take this runner's own live claim comments off `number`'s issue, and the claimed label
/// unless another runner's live claim is still there: a run gives back an item it claimed.
/// A failure is reported on stderr and nothing more.
pub fn release_own_claim(home: &Home, checkout: &Path, number: u64) {
    free_claim(home, checkout, number, Freeing::Own);
}

/// What freeing does with an issue's live claim comments, oldest first: the ids to delete and
/// the claim that stays, if any, which keeps the item that runner's, label included. With `me`
/// None every claim goes. With `me`, this runner's own go, and so does every other runner's
/// posted after its first one: those lost the race (the first live claim wins), and one left
/// behind would win every later runner's race check while nobody builds the item. A claim
/// posted before this runner's first stays; the first of those (the race's winner) holds the
/// item.
fn to_free(live: &[ClaimComment], me: Option<&str>, escalated: bool) -> (Vec<u64>, Option<Claim>) {
    let first_mine = match me {
        None => 0,
        Some(me) => live
            .iter()
            .position(|c| c.runner == me)
            .unwrap_or(live.len()),
    };
    let (ahead, rest) = live.split_at(first_mine);
    let delete = rest.iter().filter_map(|c| c.id).collect();
    // `live` holds live claims only, so the first ahead is the race's winner.
    let stays = ahead.first().map(|c| held(c, escalated));
    (delete, stays)
}

fn free_claim(home: &Home, checkout: &Path, number: u64, freeing: Freeing) {
    use std::io::Write as _;
    let mut failures = Vec::new();
    let result = (|| -> Result<()> {
        let ok = crate::ahjo::untampered(home, checkout)?;
        let labels = Labels::of_default_branch(&ok)?;
        let me = match freeing {
            Freeing::Every => None,
            Freeing::Own => Some(runner_id(home)?),
        };
        // Looked at first: the label may be gone already, or this repository may never have
        // had it, and removing a label that doesn't exist is an error.
        let view = view_issue(&ok, number)?;
        // The live claim comments go too: left behind, one would win every other runner's race
        // check until it went stale.
        let now = now_secs();
        let live: Vec<ClaimComment> = view
            .claims()
            .into_iter()
            .filter(|c| !c.is_stale(now))
            .collect();
        let escalated = view.has_label(&labels.needs_answer);
        let (delete, stays) = to_free(&live, me.as_deref(), escalated);
        for id in delete {
            let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
            // Carry on past a delete that fails (another account's comment answers 403), so
            // the label still comes off and the rest still go.
            if let Err(err) = gh(&ok, &["api", "-X", "DELETE", &path]) {
                failures.push(format!("deleting claim comment {id}: {err:#}"));
            }
        }
        let name = name_of(&ok, None)?;
        let labelled = view.has_label(&labels.claimed());
        if stays.is_none() && labelled {
            remove_label(&ok, number, &labels.claimed())?;
        }
        // Recorded as a sync would read it: an issue without the label holds nothing.
        let stays = stays.filter(|_| labelled);
        let lock = home.lock()?;
        home.events()
            .append(&lock, EV_CLAIM, claim_event(&name, number, stays.as_ref()))?;
        Ok(())
    })();
    if let Err(err) = result {
        failures.push(format!("{err:#}"));
    }
    if !failures.is_empty() {
        let _ = writeln!(
            std::io::stderr(),
            "paja: couldn't free #{number}'s claim on GitHub: {}; remove its claimed label and \
             any live `Claimed by runner` comment by hand so other runners can take it",
            failures.join("; ")
        );
    }
}

/// What a `paja run` could take now, in the order it takes them: the items earlier runs left
/// active whose run is gone, then the ready ones. Items a live run holds are left out. A pool
/// hands these to its lanes; the lanes' own runs claim them.
pub fn runnable(home: &Home, repo: &str) -> Result<Vec<WorkItem>> {
    let lock = home.lock()?;
    let log = home.events();
    let events = pin_legacy(&lock, &log, repo)?;
    let (left, ready) = run_candidates(&events, repo)?;
    let mut free = Vec::new();
    for item in left.into_iter().chain(ready) {
        // Probed under the home lock, under which runs claim, so a claim never meets the probe.
        if home.try_run_lock(repo, item.number)?.is_some() {
            free.push(item);
        }
    }
    Ok(free)
}

/// The numbers of the items an earlier `paja run` left active in `repo` and no live run holds:
/// the ones a run without `--item` resumes first.
pub fn left_active(home: &Home, repo: &str) -> Result<Vec<u64>> {
    let lock = home.lock()?;
    let log = home.events();
    let events = pin_legacy(&lock, &log, repo)?;
    let (left, _) = run_candidates(&events, repo)?;
    let mut free = Vec::new();
    for item in left {
        if home.try_run_lock(repo, item.number)?.is_some() {
            free.push(item.number);
        }
    }
    Ok(free)
}

/// The items of `repo` a run may take, before their run locks are looked at: those an earlier
/// `paja run` left active (its latest status change was a run's claim), and the ready ones in
/// priority order. Items a person or another worker claimed with `paja work claim` are in
/// neither.
fn run_candidates(events: &[Event], repo: &str) -> Result<(Vec<WorkItem>, Vec<WorkItem>)> {
    // Which items' latest status change was a run's claim.
    let mut by_run = BTreeMap::new();
    // Replaying first refuses a status change it can't read, so every one parses here. Only
    // this repository's items are ever taken, whatever the log holds of others.
    let items = replay_repo(events, repo)?;
    let legacy = legacy_repo(events, repo);
    for event in events
        .iter()
        .filter(|e| e.kind == EV_STATUS && event_repo(e, legacy) == repo)
    {
        if let Ok(change) = serde_json::from_value::<StatusChange>(event.data.clone()) {
            by_run.insert(change.number, change.run && change.status == Status::Active);
        }
    }
    let left = items
        .values()
        .filter(|item| {
            item.open && item.status == Status::Active && by_run.get(&item.number) == Some(&true)
        })
        .cloned()
        .collect();
    // An item another runner holds on GitHub is that runner's until its claim goes stale.
    let now = now_secs();
    let mut ready: Vec<WorkItem> = items
        .into_values()
        .filter(is_ready)
        .filter(|item| held_by_other(events, item, now).is_none())
        .collect();
    ready.sort_by_key(|item| item.priority);
    Ok((left, ready))
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
    let events = pin_legacy(&lock, &log, repo)?;
    let mut item = replay_repo(&events, repo)?
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
    let name = name_of(&ok, None)?;
    let mut listed = fetch_issues(&ok, &labels.work)?;
    if rules.follow_ups {
        // An issue with both labels is a work item.
        let mut follow_ups = fetch_issues(&ok, &labels.follow_up)?;
        follow_ups.retain(|f| !listed.iter().any(|i| i.issue.number == f.issue.number));
        for f in &mut follow_ups {
            f.issue.priority = Priority::FollowUp;
        }
        listed.extend(follow_ups);
    }
    // Who holds each item on GitHub: asked of the issues that carry the claimed label only.
    let claimed = labels.claimed();
    let mut held: BTreeMap<u64, Claim> = BTreeMap::new();
    for l in &listed {
        if l.labels.iter().any(|label| label.name == claimed) {
            if let Some(claim) = holder(&view_issue(&ok, l.issue.number)?, &labels) {
                held.insert(l.issue.number, claim);
            }
        }
    }
    let seen: BTreeSet<u64> = listed.iter().map(|l| l.issue.number).collect();
    let issues: Vec<Issue> = listed.into_iter().map(|l| l.issue).collect();
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
    // With follow-ups off, an imported follow-up that wasn't listed is hidden by that rule only
    // while its issue is still open on GitHub; one closed there closes as any item does.
    let mut hidden = BTreeSet::new();
    if !rules.follow_ups {
        for item in replay_repo(&home.events().read_all()?, &name)?.values() {
            if item.open
                && item.priority == Priority::FollowUp
                && !queued.contains(&item.number)
                && issue_is_open(&ok, item.number)?
            {
                hidden.insert(item.number);
            }
        }
    }
    // With follow-ups on again, a landed follow-up that the rule hid is asked about once: if
    // GitHub reopened it while it was hidden (it may have closed and reopened in between), it
    // needs work again.
    let mut reopened = BTreeSet::new();
    if rules.follow_ups {
        for (number, hidden_at) in landed_hidden(&home.events().read_all()?, &name)? {
            if queued.contains(&number) && reopened_since(&ok, number, hidden_at)? {
                reopened.insert(number);
            }
        }
    }
    let report = apply_listed(home, &name, &issues, &open_outside, &hidden, &reopened)?;
    apply_claims(home, &name, &seen, &held)?;
    record_checkout(home, &name, &checkout_top(repo)?)?;
    Ok(report)
}

/// The top level of the checkout `path` is in (the nearest folder holding `.git`), so a sync
/// from a subdirectory names the same checkout as one from its root. Found from the path's own
/// spelling, not from git, so it compares equal to the path an environment records; `..` parts
/// are folded first, so `/a/b/../c` is `/a/c`.
fn checkout_top(path: &Path) -> Result<std::path::PathBuf> {
    let path = fold_parents(&std::path::absolute(path)?);
    Ok(path
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map_or_else(|| path.clone(), Path::to_path_buf))
}

/// `path` with each `..` part taking away the part before it, as a shell's `cd` does, without
/// asking the file system. A `..` at the root stays at the root.
fn fold_parents(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut folded = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                if matches!(folded.components().next_back(), Some(Component::Normal(_))) {
                    folded.pop();
                }
            }
            Component::CurDir => {}
            other => folded.push(other),
        }
    }
    folded
}

/// Record that `path` is a checkout of `repo`, unless it is already the last one recorded. A
/// path that isn't UTF-8 can't be written to the log as text, so it isn't recorded: Paja Desk
/// then finds the checkout only by starting there.
fn record_checkout(home: &Home, repo: &str, path: &Path) -> Result<()> {
    let path = std::path::absolute(path)?;
    let Some(text) = path.to_str() else {
        return Ok(());
    };
    let lock = home.lock()?;
    let log = home.events();
    let last = log
        .read_all()?
        .into_iter()
        .rev()
        .find(|e| e.kind == EV_CHECKOUT && e.data["repo"] == repo);
    // The same directory spelled another way (through a symlink, a Windows short name) is
    // not news either.
    if last.is_some_and(|e| {
        e.data["path"]
            .as_str()
            .is_some_and(|recorded| same_dir(Path::new(recorded), &path))
    }) {
        return Ok(());
    }
    log.append(&lock, EV_CHECKOUT, json!({ "repo": repo, "path": text }))?;
    Ok(())
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
    apply_listed(
        home,
        repo,
        open,
        open_outside,
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
}

/// The landed follow-ups of `repo` whose last close came from `follow-ups = off`, each with
/// when (milliseconds since the epoch) that close was written.
fn landed_hidden(events: &[Event], repo: &str) -> Result<BTreeMap<u64, u64>> {
    let (items, hidden) = replay_with_hidden(events, repo)?;
    let legacy = legacy_repo(events, repo);
    let mut found = BTreeMap::new();
    for (r, number) in hidden {
        let landed = items
            .get(&(r.clone(), number))
            .is_some_and(|item| item.status == Status::Landed);
        if r != repo || !landed {
            continue;
        }
        let closed = events.iter().rev().find(|event| {
            event.kind == EV_CLOSED
                && event.data["number"] == number
                && event_repo(event, legacy) == repo
        });
        if let Some(closed) = closed {
            found.insert(number, closed.ts_ms);
        }
    }
    Ok(found)
}

/// Whether GitHub reopened issue `number` at or after `since` (milliseconds since the epoch, cut
/// to the second GitHub's times have), read from the issue's events. A listing gh can't give
/// fails the sync, so the next one asks again instead of the item staying landed unseen.
/// Cutting to the second leans toward a reopen: one up to a second before the hiding counts too.
/// `since` is this machine's clock and the events GitHub's; a skew between them matters little,
/// since the issue was open on GitHub when it was hidden, so a reopen after that needs a close
/// first.
fn reopened_since(ok: &Untampered, number: u64, since: u64) -> Result<bool> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{number}/events");
    let listed = gh(
        ok,
        &[
            "api",
            &path,
            "--paginate",
            "--jq",
            ".[] | select(.event == \"reopened\") | .created_at",
        ],
    )
    .with_context(|| {
        format!(
            "asking GitHub whether follow-up #{number}, hidden while follow-ups were off, was \
             reopened since; the next `paja work sync` asks again"
        )
    })?;
    Ok(listed
        .lines()
        .filter_map(parse_utc)
        .any(|at| at >= since / 1000))
}

/// [`apply_with`], where `hidden` are the follow-ups the sync didn't list only because
/// `follow-ups = off` (their issues are still open on GitHub). Each that closes is recorded as
/// closed by that rule, so a landed one isn't queued again when follow-ups come back on, unless
/// it is in `reopened`: GitHub reopened it while it was hidden.
fn apply_listed(
    home: &Home,
    repo: &str,
    open: &[Issue],
    open_outside: &BTreeSet<u64>,
    hidden: &BTreeSet<u64>,
    reopened: &BTreeSet<u64>,
) -> Result<SyncReport> {
    let lock = home.lock()?;
    let log = home.events();
    let events = pin_legacy(&lock, &log, repo)?;
    let (all, closed_by_rule) = replay_with_hidden(&events, repo)?;
    let items: BTreeMap<u64, WorkItem> = all
        .into_iter()
        .filter(|((r, _), _)| r == repo)
        .map(|((_, number), item)| (number, item))
        .collect();
    let back_from_rules: BTreeSet<u64> = closed_by_rule
        .into_iter()
        .filter(|(r, number)| r == repo && !reopened.contains(number))
        .map(|(_, number)| number)
        .collect();
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
                    let mut data = json!({ "repo": repo, "number": issue.number });
                    if reopened.contains(&issue.number) {
                        data[REOPENED_ON_GITHUB] = json!(true);
                    }
                    log.append(&lock, EV_REOPENED, data)?;
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
                let reopened = items.get(n).is_some_and(|i| !i.open)
                    && !back_from_rules.contains(n)
                    && open.iter().any(|i| i.number == *n);
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
            let mut data = json!({ "repo": repo, "number": item.number });
            if item.priority == Priority::FollowUp && hidden.contains(&item.number) {
                data[FOLLOW_UPS_OFF] = json!(true);
            }
            log.append(&lock, EV_CLOSED, data)?;
            report.closed.push(item.number);
        }
    }
    Ok(report)
}

/// Whether the log says this runner holds item `number` of `repo`.
fn item_claimed_here(home: &Home, repo: &str, number: u64) -> bool {
    let Ok(events) = home.events().read_all() else {
        return false;
    };
    let Ok(items) = replay_repo(&events, repo) else {
        return false;
    };
    items
        .get(&number)
        .and_then(|item| item.claim.as_ref())
        .is_some_and(|claim| Some(claim.runner.as_str()) == runner_of(&events))
}

/// Record what a sync read of the claims on `seen`, the issues it listed: `held` holds the claim
/// of each that carries the claimed label. Only a change is recorded.
fn apply_claims(
    home: &Home,
    repo: &str,
    seen: &BTreeSet<u64>,
    held: &BTreeMap<u64, Claim>,
) -> Result<()> {
    let lock = home.lock()?;
    let log = home.events();
    let items = replay_repo(&log.read_all()?, repo)?;
    for number in seen {
        let Some(item) = items.get(number) else {
            continue;
        };
        if item.claim.as_ref() != held.get(number) {
            log.append(
                &lock,
                EV_CLAIM,
                claim_event(repo, *number, held.get(number)),
            )?;
        }
    }
    Ok(())
}

/// Record the arbiter agent's answer to an escalation Mestari didn't take to the person, and
/// queue the item again, in one step under the home lock. Like [`answer`] it only touches the
/// event log; the caller comments on the issue.
pub fn answer_by_arbiter(
    home: &Home,
    repo: &str,
    number: u64,
    question: &str,
    text: &str,
) -> Result<()> {
    let lock = home.lock()?;
    let log = home.events();
    pin_legacy(&lock, &log, repo)?;
    log.append(
        &lock,
        EV_ANSWERED,
        json!({
            "repo": repo, "number": number, "question": question, "answer": text,
            "by": BY_ARBITER
        }),
    )?;
    log.append(
        &lock,
        EV_STATUS,
        json!({ "repo": repo, "number": number, "status": Status::Queued, "note": null }),
    )?;
    Ok(())
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
    let name = name_of(&ok, None)?;
    let item = {
        let lock = home.lock()?;
        let log = home.events();
        let events = pin_legacy(&lock, &log, &name)?;
        let mut item = replay_repo(&events, &name)?
            .remove(&number)
            .with_context(|| no_item(&name, number))?;
        // A queued item whose last answer is the arbiter's hasn't run since: a person may
        // override that answer, which then comes last in the prompts.
        let overriding =
            item.status == Status::Queued && item.answers.last().is_some_and(Answer::by_arbiter);
        if item.status != Status::Escalated && !overriding {
            bail!(
                "work item #{number} is {}, not escalated, so there is no question to answer; \
                 check `paja work show {number}`",
                item.status.as_str()
            );
        }
        let answer = Answer {
            question: if overriding {
                item.answers.last().and_then(|a| a.question.clone())
            } else {
                item.note.clone()
            },
            answer: text.to_string(),
            by: None,
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
    // A `.paja/labels` that doesn't read leaves the default label to try, and says so: the
    // configured one then stays on the issue until the file is fixed.
    let needs_answer = Labels::of_default_branch(&ok)
        .map(|labels| labels.needs_answer)
        .unwrap_or_else(|err| {
            let fallback = Labels::default().needs_answer;
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "paja: {err:#}; removing the default `{fallback}` label from #{number} instead"
            );
            fallback
        });
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
        // Not `eprintln!`: a closed stderr (Paja Desk, `paja mcp`) must not abort the answer.
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "paja: couldn't remove the `{needs_answer}` label from #{number}: {err:#}"
        );
    }
    // The item stays claimed by this runner, which runs it next: a claim that went stale while
    // the item waited is renewed, so no other runner takes it first. Only when the item holds a
    // claim at all, so a repository that never claimed it gains none.
    if item_claimed_here(home, &name, number) {
        if let Err(err) = renew_claim(home, repo, number) {
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "paja: couldn't renew #{number}'s claim on GitHub: {err:#}"
            );
        }
    }
    // Redacted like Paja's own comments: an agent can write the answer (`/paja:answer`, the
    // MCP tool), so tokens and local paths stay off the public issue and mentions are inert.
    // From a file: a long answer would not fit a Windows command line.
    let top = crate::ahjo::top_level(&ok).unwrap_or_else(|_| ok.path().to_path_buf());
    let text =
        crate::mestari::redact_for_comment(text, &top, home.root(), crate::mestari::QUOTED_CHARS);
    let body = format!("**Answer to Paja's question.**\n\n{text}");
    let posted =
        crate::mestari::body_file(home, &format!("answer-{number}"), &body).and_then(|file| {
            let posted = gh(
                &ok,
                &[
                    "issue",
                    "comment",
                    &number.to_string(),
                    "--body-file",
                    &file.to_string_lossy(),
                ],
            );
            let _ = std::fs::remove_file(&file);
            posted
        });
    posted.with_context(|| {
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
    add_label(
        &ok,
        number,
        &label,
        "d93f0b",
        "Paja is waiting for an answer: paja work answer <number> \"<answer>\"",
    )?;
    Ok(label)
}

/// The local checkout of `repo` (`owner/name`) that Paja last worked in: the checkout it last
/// synced in, or the repository an environment for one of its items was made from, newest
/// first, when it still exists. None when neither happened yet. For a front end that starts outside any
/// checkout, such as Paja Desk, so it can run the core's commands where `gh` finds `repo`.
pub fn known_checkout(home: &Home, repo: &str) -> Result<Option<std::path::PathBuf>> {
    Ok(known_checkouts(home, repo)?.into_iter().next())
}

/// Every checkout [`known_checkout`] could name for `repo`, newest first, each once: a caller
/// that finds the newest one stale (its remote changed) can try the next.
pub fn known_checkouts(home: &Home, repo: &str) -> Result<Vec<std::path::PathBuf>> {
    let events = home.events().read_all()?;
    let prefix = env_name(repo, 0);
    let prefix = prefix.strip_suffix('0').unwrap_or(&prefix);
    Ok(events
        .iter()
        .rev()
        .filter_map(|e| match e.kind.as_str() {
            EV_CHECKOUT
                if e.data["repo"]
                    .as_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(repo)) =>
            {
                e.data["path"].as_str()
            }
            crate::ahjo::EV_CREATED
                if e.data["name"].as_str().is_some_and(|name| {
                    name.strip_prefix(prefix)
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                }) =>
            {
                e.data["repo"].as_str()
            }
            _ => None,
        })
        .map(std::path::PathBuf::from)
        .fold(
            Vec::new(),
            |mut kept: Vec<(std::path::PathBuf, std::path::PathBuf)>, path| {
                // Each directory once, however it was spelled: the newest spelling is kept. Each
                // spelling is resolved once; one that can't be is no directory now.
                if !kept.iter().any(|(known, _)| *known == path) {
                    if let Ok(resolved) = std::fs::canonicalize(&path) {
                        if resolved.is_dir() && !kept.iter().any(|(_, r)| *r == resolved) {
                            kept.push((path, resolved));
                        }
                    }
                }
                kept
            },
        )
        .into_iter()
        .map(|(path, _)| path)
        .collect())
}

/// Whether `a` and `b` name the same directory: as resolved by the file system (symlinks,
/// Windows short names and letter case where it is ignored), or spelled alike when either
/// can't be resolved.
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
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
    gh_output_within(ok, args, None)
}

/// `gh args` in the checkout. With a `limit`, gh is stopped once it has run that long, and
/// that is an error.
fn gh_output_within(
    ok: &Untampered,
    args: &[&str],
    limit: Option<std::time::Duration>,
) -> Result<std::process::Output> {
    let gh = gh_program();
    let mut cmd = Command::new(&gh);
    crate::platform::own_process_group(cmd.env("GH_PROMPT_DISABLED", "1"))
        .current_dir(ok.path())
        .args(args);
    match limit {
        None => cmd.output().with_context(|| {
            format!("running `{gh}` (install the GitHub CLI and run `gh auth login`)")
        }),
        Some(limit) => crate::platform::output_within(&mut cmd, limit).with_context(|| {
            format!(
                "running `{gh} {}` (if it didn't start, install the GitHub CLI and run `gh \
                     auth login`)",
                args.join(" ")
            )
        }),
    }
}

pub(crate) fn gh(ok: &Untampered, args: &[&str]) -> Result<String> {
    gh_within(ok, args, None)
}

pub(crate) fn gh_within(
    ok: &Untampered,
    args: &[&str],
    limit: Option<std::time::Duration>,
) -> Result<String> {
    let repo = ok.path();
    if !repo.is_dir() {
        bail!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            repo.display()
        );
    }
    let gh = gh_program();
    let output = gh_output_within(ok, args, limit)?;
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

/// An issue with the labels it carries, as `gh issue list --json` prints it.
#[derive(Deserialize)]
struct Listed {
    #[serde(flatten)]
    issue: Issue,
    #[serde(default)]
    labels: Vec<LabelName>,
}

fn fetch_issues(ok: &Untampered, label: &str) -> Result<Vec<Listed>> {
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
            "number,title,body,url,labels",
        ],
    )?;
    if !output.status.success() {
        bail!(
            "`{gh} issue list` failed in {}: {}; check `gh auth status` and that this checkout has a GitHub remote",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let issues: Vec<Listed> = serde_json::from_slice(&output.stdout).with_context(|| {
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
    fn the_holder_is_the_race_winner_not_a_loser_left_behind() {
        let labels = Labels::default();
        let comment = |runner: &str, stale: &str, id: u64| {
            json!({
                "body": format!("Claimed by runner {runner} at 2020-01-01T00:00:00Z; stale after {stale}"),
                "url": format!("https://example.com/acme/app/issues/5#issuecomment-{id}"),
            })
        };
        let view = |comments: Value| -> View {
            serde_json::from_value(json!({
                "labels": [{"name": labels.claimed()}],
                "comments": comments,
            }))
            .unwrap()
        };
        // A stale claim, the winner, then a loser whose comment couldn't be deleted.
        let raced = view(json!([
            comment("old", "2020-01-02T00:00:00Z", 1),
            comment("winner", "2999-01-01T00:00:00Z", 2),
            comment("loser", "2999-01-01T00:00:00Z", 3),
        ]));
        assert_eq!(holder(&raced, &labels).unwrap().runner, "winner");
        // With every claim stale, the latest still names who held it last.
        let stale = view(json!([
            comment("a", "2020-01-02T00:00:00Z", 1),
            comment("b", "2020-01-03T00:00:00Z", 2),
        ]));
        assert_eq!(holder(&stale, &labels).unwrap().runner, "b");
    }

    #[test]
    fn claim_times_round_trip_and_claim_comments_read_back() {
        for secs in [0, 951_782_400, 1_785_000_123, 4_102_444_799] {
            assert_eq!(parse_utc(&format_utc(secs)), Some(secs), "{secs}");
        }
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_utc("yesterday"), None);
        let text = claim_text("0a1b2c3d4e5f6071", 1_785_000_000);
        let url = "https://github.com/a/b/issues/5#issuecomment-4242";
        let claim = parse_claim_comment(&text, url).unwrap();
        assert_eq!(claim.runner, "0a1b2c3d4e5f6071");
        assert_eq!(claim.id, Some(4242));
        assert!(!claim.is_stale(1_785_000_000 + CLAIM_SECS - 1));
        assert!(claim.is_stale(1_785_000_000 + CLAIM_SECS));
        assert!(parse_claim_comment("Claimed by someone", url).is_none());
        assert!(parse_claim_comment("hello", url).is_none());
    }

    #[test]
    fn freeing_deletes_every_live_claim_or_only_this_runners() {
        let comment = |runner: &str, id: u64| {
            let url = format!("https://github.com/a/b/issues/5#issuecomment-{id}");
            parse_claim_comment(&claim_text(runner, 1_785_000_000), &url).unwrap()
        };
        let live = [comment("bbbb", 1), comment("aaaa", 2), comment("cccc", 3)];
        // The person put the item back: every claim goes, and nothing stays.
        assert_eq!(to_free(&live, None, false), (vec![1, 2, 3], None));
        // A run gives back its own: runner bbbb's earlier claim stays, and with it the label;
        // cccc's later one lost the race to aaaa and goes with it.
        let (delete, stays) = to_free(&live, Some("aaaa"), false);
        assert_eq!(delete, vec![2, 3]);
        assert_eq!(stays.unwrap().runner, "bbbb");
        // With no earlier claim, nothing stays and the label comes off too.
        let (delete, stays) = to_free(&live[1..], Some("aaaa"), false);
        assert_eq!((delete, stays), (vec![2, 3], None));
        // A runner with no live claim of its own deletes nothing; the race's winner holds.
        let (delete, stays) = to_free(&live, Some("dddd"), false);
        assert!(delete.is_empty());
        assert_eq!(stays.unwrap().runner, "bbbb");
        // Two claims ahead of this runner's (cccc lost to bbbb, its comment left behind): the
        // earlier one holds the item.
        let raced = [comment("bbbb", 1), comment("cccc", 2), comment("aaaa", 3)];
        let (delete, stays) = to_free(&raced, Some("aaaa"), false);
        assert_eq!(delete, vec![3]);
        assert_eq!(stays.unwrap().runner, "bbbb");
        // An escalated item's claim stays with no stale time, as a sync records it.
        let (_, stays) = to_free(&raced, Some("aaaa"), true);
        assert_eq!(stays.unwrap().stale_after, None);
    }

    #[test]
    fn a_race_is_won_only_by_a_live_own_comment_that_comes_first() {
        let comment = |runner: &str, id: u64| {
            let url = format!("https://github.com/a/b/issues/5#issuecomment-{id}");
            parse_claim_comment(&claim_text(runner, 1_785_000_000), &url).unwrap()
        };
        let now = 1_785_000_001;
        let mine = [comment("aaaa", 2)];
        assert_eq!(race_result(&mine, "aaaa", Some(2), now), Race::Won);
        // Without the comment's id (gh printed no URL) it is found by runner.
        assert_eq!(race_result(&mine, "aaaa", None, now), Race::Won);
        let lost = [comment("bbbb", 1), comment("aaaa", 2)];
        match race_result(&lost, "aaaa", Some(2), now) {
            Race::LostTo(c) => assert_eq!(c.runner, "bbbb"),
            other => panic!("{other:?}"),
        }
        // A runner giving the item back deleted this one's comment as a later claim: no win.
        assert_eq!(race_result(&[], "aaaa", Some(2), now), Race::Gone);
        assert_eq!(
            race_result(&[comment("aaaa", 1)], "aaaa", Some(2), now),
            Race::Gone
        );
        // This runner's own comment, gone stale, wins nothing.
        assert_eq!(
            race_result(&mine, "aaaa", Some(2), now + CLAIM_SECS),
            Race::Gone
        );
    }

    #[test]
    fn taking_back_a_claim_keeps_only_an_earlier_runners_label() {
        let comment = |runner: &str, id: u64| {
            let url = format!("https://github.com/a/b/issues/5#issuecomment-{id}");
            parse_claim_comment(&claim_text(runner, 1_785_000_000), &url).unwrap()
        };
        let now = 1_785_000_001;
        let earlier = [comment("bbbb", 1), comment("aaaa", 2)];
        assert!(label_is_another_runners(&earlier, "aaaa", Some(2), now));
        // A later claim lost the race to this runner's comment.
        let later = [comment("aaaa", 2), comment("cccc", 3)];
        assert!(!label_is_another_runners(&later, "aaaa", Some(2), now));
        // With no comment of its own to order against, any other live claim keeps the label.
        assert!(label_is_another_runners(&later, "aaaa", None, now));
        // A stale claim holds nothing.
        assert!(!label_is_another_runners(
            &earlier,
            "aaaa",
            Some(2),
            now + CLAIM_SECS
        ));
        assert!(!label_is_another_runners(
            &[comment("aaaa", 2)],
            "aaaa",
            None,
            now
        ));
    }

    #[test]
    fn only_a_missing_label_is_taken_for_one() {
        let missing = anyhow::anyhow!("`gh issue edit` failed: could not add label: 'x' not found");
        assert!(label_missing(&missing));
        let down = anyhow::anyhow!("`gh issue edit` failed: HTTP 502: Bad Gateway");
        assert!(!label_missing(&down));
        let elsewhere =
            anyhow::anyhow!("`gh issue edit` failed in /src/labeler: HTTP 404: Not Found");
        assert!(!label_missing(&elsewhere));
        let gone = anyhow::anyhow!(
            "`gh issue edit` failed: GraphQL: Could not resolve to an issue or pull request"
        );
        assert!(!label_missing(&gone));
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
    fn a_checkout_top_folds_parents_and_finds_the_nearest_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::path::absolute(dir.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        // A `..` part is folded away, so the top is spelled as an environment records it.
        let roundabout = repo.join("src").join("..").join(".").join("src");
        assert_eq!(checkout_top(&roundabout).unwrap(), repo);
        // A linked worktree's `.git` is a file.
        let linked = root.join("linked");
        std::fs::create_dir_all(linked.join("deep")).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(checkout_top(&linked.join("deep")).unwrap(), linked);
        // No `.git` above it: the path itself. A temporary directory that is itself inside
        // some checkout finds that one instead, and the fallback isn't exercised then.
        let bare = root.join("bare").join("x");
        std::fs::create_dir_all(&bare).unwrap();
        let above = root
            .ancestors()
            .find(|dir| dir.join(".git").exists())
            .map_or_else(|| bare.clone(), Path::to_path_buf);
        assert_eq!(checkout_top(&bare.join("..").join("x")).unwrap(), above);
    }

    #[test]
    fn fold_parents_takes_away_the_part_before_each_dot_dot() {
        let path = |parts: &[&str]| parts.iter().collect::<std::path::PathBuf>();
        let top = std::path::absolute("/").unwrap();
        assert_eq!(
            fold_parents(&top.join(path(&["a", "b", "..", "c"]))),
            top.join(path(&["a", "c"]))
        );
        assert_eq!(fold_parents(&top.join(path(&["..", "a"]))), top.join("a"));
        assert_eq!(
            fold_parents(&top.join(path(&["a", ".", "b"]))),
            top.join(path(&["a", "b"]))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_checkout_path_that_is_not_utf8_is_not_recorded() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let odd = dir.path().join(std::ffi::OsStr::from_bytes(b"app\xff"));
        record_checkout(&home, R, &odd).unwrap();
        assert!(home.events().read_all().unwrap().is_empty());
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
        // Every one still there, newest first, each once.
        let lock = home.lock().unwrap();
        log.append(&lock, crate::ahjo::EV_CREATED, env(R, 1, old))
            .unwrap();
        drop(lock);
        assert_eq!(
            known_checkouts(&home, R).unwrap(),
            vec![dir.path().join(old), dir.path().join(new)]
        );
    }

    /// A checkout reached through a symlink is the same checkout: recorded once, named once.
    #[cfg(unix)]
    #[test]
    fn a_checkout_spelled_through_a_symlink_is_recorded_and_named_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        record_checkout(&home, R, &real).unwrap();
        record_checkout(&home, R, &link).unwrap();
        let recorded = |home: &Home| {
            home.events()
                .read_all()
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EV_CHECKOUT)
                .count()
        };
        assert_eq!(recorded(&home), 1);
        // Both spellings in the log, as an older Paja recorded them: named once, the newest.
        let lock = home.lock().unwrap();
        home.events()
            .append(&lock, EV_CHECKOUT, json!({ "repo": R, "path": link }))
            .unwrap();
        drop(lock);
        assert_eq!(known_checkouts(&home, R).unwrap(), vec![link]);
    }

    /// On Windows a checkout spelled in other letter case is the same checkout.
    #[cfg(windows)]
    #[test]
    fn a_checkout_spelled_in_other_letter_case_is_recorded_and_named_once() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let real = std::path::absolute(dir.path()).unwrap().join("Real");
        std::fs::create_dir_all(&real).unwrap();
        let other = std::path::PathBuf::from(real.to_str().unwrap().to_uppercase());
        assert_ne!(other, real);
        record_checkout(&home, R, &real).unwrap();
        record_checkout(&home, R, &other).unwrap();
        let recorded = home
            .events()
            .read_all()
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == EV_CHECKOUT)
            .count();
        assert_eq!(recorded, 1);
        assert_eq!(known_checkouts(&home, R).unwrap(), vec![real]);
    }

    #[test]
    fn a_synced_checkout_counts_and_the_newest_existing_one_wins() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        for name in ["env-repo", "synced", "other"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        let lock = home.lock().unwrap();
        let log = home.events();
        let env = json!({
            "name": env_name(R, 1), "repo": dir.path().join("env-repo"),
            "path": dir.path().join("env"), "branch": "b", "base_commit": "0",
            "port_base": 41000, "port_count": 10, "created_ms": 0,
        });
        log.append(&lock, crate::ahjo::EV_CREATED, env).unwrap();
        let checkout =
            |repo: &str, path: &str| json!({ "repo": repo, "path": dir.path().join(path) });
        log.append(&lock, EV_CHECKOUT, checkout(R, "synced"))
            .unwrap();
        log.append(&lock, EV_CHECKOUT, checkout("acme/lib", "other"))
            .unwrap();
        drop(lock);
        // A sync newer than the environment wins; another repository's never counts.
        assert_eq!(
            known_checkout(&home, R).unwrap(),
            Some(dir.path().join("synced"))
        );
        assert_eq!(
            known_checkout(&home, "acme/lib").unwrap(),
            Some(dir.path().join("other"))
        );
        // Once its folder is gone, the older environment's repository is next.
        std::fs::remove_dir(dir.path().join("synced")).unwrap();
        assert_eq!(
            known_checkout(&home, R).unwrap(),
            Some(dir.path().join("env-repo"))
        );
    }

    #[test]
    fn a_checkout_is_found_whatever_the_case_of_the_repository_name() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("synced")).unwrap();
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                EV_CHECKOUT,
                json!({ "repo": R, "path": dir.path().join("synced") }),
            )
            .unwrap();
        drop(lock);
        assert_eq!(
            known_checkouts(&home, "Acme/App").unwrap(),
            vec![dir.path().join("synced")]
        );
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
        // Other environments of a repository share its name and hash, and differ by what.
        let triage = env_name_for("acme/app", "triage");
        assert_eq!(triage, a.replace("-work-5", "-triage"));
        for name in [
            a,
            b,
            env_name("Some-Owner/A_Very.Long-Repository-Name", u64::MAX),
            env_name("o/___", 1),
            env_name_for("acme/app", "triage"),
            env_name_for("Some-Owner/A_Very.Long-Repository-Name", "triage"),
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
    fn a_landed_follow_up_hidden_by_follow_ups_off_stays_landed_but_a_real_reopen_requeues() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let follow_up = |number: u64| Issue {
            priority: Priority::FollowUp,
            ..issue(number, "tidy")
        };
        let both = [issue(1, "a"), follow_up(2), follow_up(3)];
        apply(&home, &both).unwrap();
        for n in [1, 2] {
            set_status(&home, n, Status::Landed, None).unwrap();
        }
        // Follow-ups off: the sync lists only the work item.
        let hidden = BTreeSet::from([2, 3]);
        let off = super::apply_listed(
            &home,
            R,
            &both[..1],
            &BTreeSet::new(),
            &hidden,
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(off.closed, vec![2, 3]);
        // A work item that depends on #2 doesn't wait on it once follow-ups are back on.
        let mut dependent = issue(4, "d");
        dependent.body = "Depends on #2".into();
        let mut on = both.to_vec();
        on.push(dependent);
        let back = apply(&home, &on).unwrap();
        assert_eq!(back.reopened, vec![2, 3]);
        assert_eq!(get(&home, 2).unwrap().status, Status::Landed);
        assert_eq!(get(&home, 3).unwrap().status, Status::Queued);
        assert!(get(&home, 4).unwrap().waiting_on.is_empty());
        // A close GitHub makes (say a revert reopens the issue later) still requeues.
        apply(&home, &on[..1]).unwrap();
        apply(&home, &on).unwrap();
        assert_eq!(get(&home, 2).unwrap().status, Status::Queued);
        // A work item closed while follow-ups are off isn't marked: it requeues as before.
        apply(&home, &on).unwrap();
        set_status(&home, 1, Status::Landed, None).unwrap();
        let hidden = BTreeSet::from([1, 2, 3]);
        super::apply_listed(&home, R, &[], &BTreeSet::new(), &hidden, &BTreeSet::new()).unwrap();
        apply(&home, &on).unwrap();
        assert_eq!(get(&home, 1).unwrap().status, Status::Queued);
        // A landed follow-up whose issue GitHub closed while follow-ups were off isn't hidden
        // by the rule, so when GitHub reopens it (a revert, say) it is queued again.
        set_status(&home, 2, Status::Landed, None).unwrap();
        let only_3 = BTreeSet::from([3]);
        super::apply_listed(
            &home,
            R,
            &on[..1],
            &BTreeSet::new(),
            &only_3,
            &BTreeSet::new(),
        )
        .unwrap();
        apply(&home, &on).unwrap();
        assert_eq!(get(&home, 2).unwrap().status, Status::Queued);
        // A landed follow-up the rule hid, which GitHub reopened meanwhile, is queued when
        // follow-ups come back on; the reopen is marked, so replay agrees.
        set_status(&home, 2, Status::Landed, None).unwrap();
        let hidden = BTreeSet::from([2, 3]);
        super::apply_listed(
            &home,
            R,
            &on[..1],
            &BTreeSet::new(),
            &hidden,
            &BTreeSet::new(),
        )
        .unwrap();
        let events = home.events().read_all().unwrap();
        let hidden_at = super::landed_hidden(&events, R).unwrap();
        assert_eq!(hidden_at.keys().copied().collect::<Vec<_>>(), vec![2]);
        let reopened = BTreeSet::from([2]);
        let back =
            super::apply_listed(&home, R, &on, &BTreeSet::new(), &BTreeSet::new(), &reopened)
                .unwrap();
        assert!(back.reopened.contains(&2));
        assert_eq!(get(&home, 2).unwrap().status, Status::Queued);
        let last = home.events().read_all().unwrap();
        let mark = last
            .iter()
            .rev()
            .find(|e| e.kind == EV_REOPENED && e.data["number"] == 2);
        assert_eq!(mark.unwrap().data[super::REOPENED_ON_GITHUB], true);
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
                    answer: "v2".into(),
                    by: None
                },
                Answer {
                    question: None,
                    answer: "Keep it small".into(),
                    by: None
                },
            ]
        );
    }
}
