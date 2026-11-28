//! Mestari: runs one work item through its graph. The first graph is the smallest loop that
//! can build Paja: build in a fresh Ahjo environment, run Vasara's checks, have independent
//! reviewer agents read the diff cold, then land it or loop back, and escalate when stuck.
//!
//! Every step is a fresh headless `claude -p` run. The checks and the high-risk paths are read
//! from the base commit on the default branch, never from the branch under review, so a change can't
//! loosen the checks that judge it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ahjo::{self, CreateOptions, Env};
use crate::home::Home;
use crate::jono::{self, Status, WorkItem};

mod agent;
mod arbiter;
mod builder;
mod detect;
mod follow_ups;
mod land;
pub mod live;
pub mod plan;
pub mod pool;
mod progress;
mod triage;
mod usage;

pub use detect::{init_agent, recommend_agent, skeleton_agent, skeleton_checks};
pub use triage::{
    triage, triage_if_due, TriageReport, Triaged, Verdict, EV_TRIAGE_FAILED, EV_TRIAGE_RAN,
};

use agent::*;
use arbiter::*;
use builder::*;
use follow_ups::*;
use land::*;
use usage::*;

pub(crate) use follow_ups::{body_file, redact_for_comment, QUOTED_CHARS};
pub use progress::progress;
use progress::*;
pub use usage::{ModelUsage, StepUsage};

/// Most build rounds before a run escalates instead of trying again.
pub const MAX_ROUNDS: u32 = 5;
/// Vasara's checks: one shell command per line, read from the base commit.
pub const CHECKS_FILE: &str = ".paja/checks";
/// Path prefixes that make a change high risk (two reviewers), read from the base commit.
pub const HIGH_RISK_FILE: &str = ".paja/high-risk";
/// The model and effort for each agent role, read from the base commit.
pub const MODELS_FILE: &str = ".paja/models";
/// Whether the repository has CI Paja waits for before landing (`required` or `none`), read from
/// the base commit.
pub const CI_FILE: &str = ".paja/ci";
/// The default branch when `gh` names none.
const FALLBACK_BRANCH: &str = "main";
/// The `gh pr merge` flags landing uses, and the summary names the same ones.
const SQUASH: &str = "--squash";
const REBASE: &str = "--rebase";
const MERGE: &str = "--merge";
/// The roles `.paja/models` may name, besides `fallback`.
pub const ROLES: [&str; 9] = [
    "build",
    "fix",
    "review",
    "re-review",
    "review-2",
    "triage",
    "plan",
    "init",
    "arbiter",
];
/// The effort levels Claude Code takes.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub const EV_STARTED: &str = "run.started";
pub const EV_ROUND: &str = "run.round";
/// A step of a round starts: `{repo, number, round, role, model, reviewer, pr}`. The role is a
/// `.paja/models` role for an agent step, `checks` for Vasara's checks, or `ci` (waiting for
/// CI) or `land` (no CI) once the pull request `pr` is open. What a finished step spent is
/// still in `run.round`. A review step also has `reviewers`, how many reviewers the round has.
pub const EV_STEP: &str = "run.step";
/// A reviewer's verdict (`land`, `changes` or `escalate`) is in: `{repo, number, round,
/// reviewer, verdict}`.
pub const EV_REVIEWED: &str = "run.reviewed";
/// A builder session is about to start: its session id, recorded before it runs, so a run
/// that is killed mid-session can resume it.
pub const EV_STEP_STARTED: &str = "run.step_started";
/// Vasara's checks passed on a commit: `{repo, number, round, commit, base, checks_passed}`.
pub const EV_CHECKED: &str = "run.checked";
/// A builder session's work was thrown away (the hook never ran), so it is never resumed.
pub const EV_STEP_DISCARDED: &str = "run.step_discarded";
pub const EV_LANDED: &str = "run.landed";
/// The files a round's change touches against the default branch, once its checks passed:
/// `{repo, number, round, paths}`.
pub const EV_CHANGED: &str = "run.changed";
pub const EV_ESCALATED: &str = "run.escalated";
/// The arbiter agent looked at an escalation before it reached the person: `{repo, number,
/// round, verdict, model}` with the `answer` (verdict `answer`: the item is queued again with it
/// recorded) or the `question` (verdict `escalate`: the person is asked it), and `overruled`
/// when Mestari turned an `answer` into `escalate`.
pub const EV_ARBITRATED: &str = "run.arbitrated";
/// A review finding the change landed without (`not filed: ...`). No issue is filed for it; older
/// logs hold ones that were.
pub const EV_FOLLOW_UP: &str = "run.follow_up";
/// The tamper lock lives in Ahjo, next to the git every command runs.
pub use crate::ahjo::{Tampered, EV_TAMPERED, EV_TAMPER_CHECKED};

/// One pass of `paja run`: how it ended, and whether it started a sandbox.
#[derive(Debug, Clone)]
pub struct Pass {
    pub report: RunReport,
    /// Whether the pass's agents and checks were to run in a sandbox. Kept here because the
    /// sandbox is in use only while the pass runs, so it can't be read afterwards.
    pub sandbox: bool,
}

/// How a run ended.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunReport {
    /// Nothing was queued.
    Idle,
    Landed {
        number: u64,
        pr: String,
        rounds: u32,
    },
    Escalated {
        number: u64,
        question: String,
        rounds: u32,
    },
    /// The arbiter agent settled an escalation: its answer is recorded and the item is queued
    /// again, so the next run goes on with it. Nobody was asked.
    Arbitrated {
        number: u64,
        answer: String,
        rounds: u32,
    },
    /// The person pressed Ctrl-C. The item stays active, and the next `paja run` resumes it in
    /// the same environment.
    Stopped { number: u64, rounds: u32 },
    /// A step hit the usage limit. The item stays active with its worktree, and the next
    /// `paja run` resumes it after `resets_at` (milliseconds since the epoch; null when Claude
    /// Code gave none).
    UsageLimit {
        number: u64,
        rounds: u32,
        resets_at: Option<u64>,
    },
    /// A recorded usage limit hasn't reset yet, so nothing was claimed or resumed.
    Paused { resets_at: u64 },
}

/// The run reached a step boundary after the person pressed Ctrl-C.
#[derive(Debug)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped by Ctrl-C")
    }
}

impl std::error::Error for Stopped {}

/// A step boundary: stop here if the person pressed Ctrl-C. The step that was running has
/// finished and been kept, so nothing is lost.
fn stop_point() -> Result<()> {
    if crate::platform::interrupts() > 0 {
        return Err(Stopped.into());
    }
    Ok(())
}

/// What a reviewer agent decides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// `land`, `changes` or `escalate`.
    pub verdict: String,
    pub summary: String,
    /// What to change, when the verdict is `changes`.
    #[serde(default)]
    pub comments: String,
    /// The intent question for a person, when the verdict is `escalate`.
    #[serde(default)]
    pub question: String,
    /// Findings that aren't blockers, each marked `bug`, `risk`, `nit` or `outside`. Mestari
    /// sends all but `outside` back to the builder as the next round's feedback; `outside` ones
    /// and any still open at the round limit are listed in the pull request. No issue is filed.
    #[serde(default)]
    pub follow_ups: Vec<Finding>,
}

/// Say on stderr that a `.paja/models` line names a role this Paja doesn't know, and that the
/// line is skipped. A warning, not an error: main may name a role a newer Paja added, and a
/// runner whose binary is older should keep working rather than fail every pass.
fn unknown_role(at: &str, role: &str) {
    progress(&format!(
        "{at}: unknown role {role:?}, so this line is skipped; this Paja knows {}. If the \
         role is new, install the newer Paja; if it is a typo, fix the line",
        ROLES.join(", ")
    ));
}

/// The model, and optionally the effort, one role runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleModel {
    pub model: String,
    pub effort: Option<String>,
}

/// `.paja/models` as it is on the default branch: one line per role, `role model [effort]`, plus an optional
/// `fallback model` line. A role with no line runs on the account's default model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Models {
    pub roles: std::collections::BTreeMap<String, RoleModel>,
    pub fallback: Option<String>,
    /// `max-turns <role> <n>` lines: the most turns one session of the role may take.
    pub max_turns: std::collections::BTreeMap<String, u32>,
    /// `window <percent>`: the share of the usage window that counts as nearly used up.
    pub window: Option<u32>,
    /// The roles of the lines skipped because this Paja doesn't know them, each once, in file
    /// order, so `paja doctor` can show them where an unattended runner's stderr would not.
    pub skipped: Vec<String>,
}

impl Models {
    /// Note a skipped line's role, once however many lines name it.
    fn skip(&mut self, role: &str) {
        if !self.skipped.iter().any(|r| r == role) {
            self.skipped.push(role.to_string());
        }
    }

    /// The used share of the usage window, in percent, from which Mestari starts no build.
    pub fn window_percent(&self) -> u32 {
        self.window.unwrap_or(90)
    }

    /// Read the file's text. `#` starts a comment line. A line naming a role this Paja doesn't
    /// know is skipped with a warning on stderr. Anything else that isn't a plain model name
    /// (and a known effort) is an error naming the line and the fix.
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Self> {
        Self::parse_on(text, "main")
    }

    /// [`Models::parse`], with errors naming the default branch the file was read from.
    pub fn parse_on(text: &str, branch: &str) -> Result<Self> {
        let mut models = Models::default();
        // A leading `-` would reach `claude` as a flag of its own.
        let plain = |word: &str| {
            !word.starts_with('-')
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']'))
        };
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = || format!("{MODELS_FILE} on {branch}, line {}: {line:?}", i + 1);
            let words: Vec<&str> = line.split_whitespace().collect();
            let (role, rest) = (words[0], &words[1..]);
            if let Some(bad) = rest.iter().find(|w| !plain(w)) {
                bail!(
                    "{}: {bad:?} is not a plain model or effort name; use names such as `sonnet` \
                     or `claude-opus-4-1` (letters, digits, `.`, `-`, `_`, `[`, `]`)",
                    at()
                );
            }
            if role == "max-turns" {
                let (Some(named), Some(Ok(limit)), 2) = (
                    rest.first(),
                    rest.get(1).map(|n| n.parse::<u32>()),
                    rest.len(),
                ) else {
                    bail!("{}: write it as `max-turns <role> <turns>`", at());
                };
                if !ROLES.contains(named) {
                    unknown_role(&at(), named);
                    models.skip(named);
                    continue;
                }
                if limit == 0 {
                    bail!("{}: a turn limit must be at least 1", at());
                }
                if models.max_turns.insert(named.to_string(), limit).is_some() {
                    bail!(
                        "{}: `max-turns {named}` is named twice; keep one line",
                        at()
                    );
                }
                continue;
            }
            if role == "window" {
                let [Ok(percent)] = rest.iter().map(|n| n.parse::<u32>()).collect::<Vec<_>>()[..]
                else {
                    bail!("{}: write it as `window <percent>`", at());
                };
                if !(1..=100).contains(&percent) {
                    bail!("{}: the window share must be from 1 to 100 percent", at());
                }
                if models.window.replace(percent).is_some() {
                    bail!("{}: `window` is named twice; keep one line", at());
                }
                continue;
            }
            if role == "fallback" {
                let [model] = rest else {
                    bail!("{}: write it as `fallback <model>`", at());
                };
                if models.fallback.replace(model.to_string()).is_some() {
                    bail!("{}: `fallback` is named twice; keep one line", at());
                }
                continue;
            }
            if !ROLES.contains(&role) {
                unknown_role(&at(), role);
                models.skip(role);
                continue;
            }
            let (model, effort) = match rest {
                [model] => (model, None),
                [model, effort] => (model, Some(effort.to_string())),
                _ => bail!("{}: write it as `{role} <model> [effort]`", at()),
            };
            if let Some(effort) = &effort {
                if !EFFORTS.contains(&effort.as_str()) {
                    bail!(
                        "{}: unknown effort {effort:?}; use one of {}",
                        at(),
                        EFFORTS.join(", ")
                    );
                }
            }
            let chosen = RoleModel {
                model: model.to_string(),
                effort,
            };
            if models.roles.insert(role.to_string(), chosen).is_some() {
                bail!("{}: the role {role} is named twice; keep one line", at());
            }
        }
        Ok(models)
    }

    /// The most turns one session of the role may take: the file's `max-turns` line, or 80 for
    /// builders and 30 for reviewers. No session runs without a cap, so an agent going in
    /// circles can't spend the account's usage window.
    pub fn max_turns(&self, role: &str) -> u32 {
        self.max_turns
            .get(role)
            .copied()
            .unwrap_or(if matches!(role, "build" | "fix") {
                80
            } else {
                30
            })
    }

    /// The model line a role runs on: its own, or for `arbiter` with no line of its own, the
    /// `review` line.
    pub fn chosen(&self, role: &str) -> Option<&RoleModel> {
        self.roles.get(role).or_else(|| {
            (role == "arbiter")
                .then(|| self.roles.get("review"))
                .flatten()
        })
    }

    /// The flags `claude -p` gets for a role. The model flags are left out when the file
    /// doesn't name the role.
    fn args(&self, role: &str) -> Vec<String> {
        let mut args = vec!["--max-turns".to_string(), self.max_turns(role).to_string()];
        let Some(chosen) = self.chosen(role) else {
            return args;
        };
        args.extend(["--model".to_string(), chosen.model.clone()]);
        if let Some(effort) = &chosen.effort {
            args.extend(["--effort".to_string(), effort.clone()]);
        }
        if let Some(fallback) = self.fallback_for(role) {
            args.extend(["--fallback-model".to_string(), fallback.to_string()]);
        }
        args
    }

    /// The `--fallback-model` a role gets: none for a role without a line (it runs on the
    /// account's default), and none for a role already on the fallback model, which Claude Code
    /// refuses.
    pub fn fallback_for(&self, role: &str) -> Option<&str> {
        let chosen = self.chosen(role)?;
        self.fallback.as_deref().filter(|f| *f != chosen.model)
    }
}

/// What an earlier round's reviewers asked for, so the next review judges only the fix.
#[derive(Deserialize)]
struct Prior {
    /// The commit those reviewers judged.
    reviewed: String,
    /// The blockers they named.
    blockers: String,
    /// Their findings outside the change, for the next reviewer to keep or drop. Findings in
    /// the change are among the fixes asked for (`blockers`), which the next review checks.
    follow_ups: Vec<Finding>,
}

/// The findings outside the change, the ones a re-review is asked to keep or drop.
fn outside(findings: &[Finding]) -> Vec<Finding> {
    findings
        .iter()
        .filter(|f| f.kind == FindingKind::Outside)
        .cloned()
        .collect()
}

pub struct RunOptions<'a> {
    /// Any path inside the repository's main checkout.
    pub repo: &'a Path,
    /// A person has checked the repository's git configuration after Paja stopped on
    /// tampering, and Paja may run git there again.
    pub tamper_checked: bool,
    /// Take only this item instead of the oldest queued one (`paja run --item`).
    pub item: Option<u64>,
    /// Ask the person, after the summary, before the first agent starts.
    pub confirm: bool,
    /// Sync Jono with GitHub before claiming. A pool's lane doesn't: its pool synced already.
    pub sync: bool,
}

/// The person did not confirm the run, so nothing was started.
#[derive(Debug)]
pub struct Declined;

impl std::fmt::Display for Declined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not confirmed; nothing was started (pass --yes to skip the question)")
    }
}

impl std::error::Error for Declined {}

/// A mistake in how `paja run` was started (a `--repo` that isn't a directory, an `--item`
/// that isn't queued): the error goes to whoever started it (the person's terminal, or the pool
/// that started a lane), so no notice goes out for it.
#[derive(Debug)]
pub struct Mistake(String);

impl std::fmt::Display for Mistake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Mistake {}

/// Whether `err` comes from how the run was started, not from a pass that ran: a [`Mistake`]
/// or a run the person [`Declined`].
pub fn is_mistake(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<Mistake>() || e.is::<Declined>())
}

/// Set an item's status by hand (`paja work set`, the `work_set` tool) in the checkout at
/// `checkout`, and do what that means outside the log: queued frees its claim on GitHub, so any
/// runner may take it; escalated, from another status, labels the issue as waiting for an
/// answer and sends a Posti notice, as a run's escalation does (set again, it only labels).
/// Neither can fail the set: what fails is said on stderr.
pub fn set_status(
    home: &Home,
    checkout: &Path,
    number: u64,
    status: jono::Status,
    note: Option<&str>,
) -> Result<jono::WorkItem> {
    let name = jono::repo_name(home, checkout)?;
    let was = jono::get(home, &name, number).map(|item| item.status).ok();
    let item = jono::set_status(home, &name, number, status, note)?;
    match item.status {
        jono::Status::Queued => jono::release_claim(home, checkout, item.number),
        jono::Status::Escalated if was != Some(jono::Status::Escalated) => {
            let notice =
                crate::posti::Notice::escalated_by_hand(&item.repo, item.number, &item.title);
            land::tell_with(home, checkout, &item, &notice);
        }
        // Already escalated: the person was told, but a label that failed the first time is
        // tried again.
        jono::Status::Escalated => {
            if let Err(err) = jono::mark_needs_answer(home, checkout, item.number) {
                progress(&format!(
                    "couldn't label #{} as waiting for an answer: {err:#}",
                    item.number
                ));
            }
        }
        _ => {}
    }
    Ok(item)
}

/// What a run is about to do, on stderr, before the first agent starts.
fn summary(item: &jono::WorkItem, target: &RepoSettings, ci: CiMode, sandbox: bool) {
    progress(&format!("repository: {}", item.repo));
    progress(&format!("item: #{} {}", item.number, item.title));
    let method = match target.merge_flag() {
        Some(SQUASH) => "by squash",
        Some(REBASE) => "by rebase",
        Some(_) => "by a merge commit",
        None => "no merge method allowed, so landing escalates",
    };
    progress(&format!(
        "merges into: {} (default branch), {method}",
        target.branch
    ));
    progress(&match ci {
        CiMode::Required => "CI: required before landing".to_string(),
        CiMode::None => format!(
            "CI: not required ({CI_FILE} says none); Vasara's checks and the reviewers decide"
        ),
    });
    if sandbox {
        progress("sandbox: in use");
    } else {
        progress(
            "no sandbox: agents and the checks run as this user, with this user's files and \
             sign-ins, and the deny-list hook is a guardrail, not a sandbox",
        );
    }
}

/// Ask on stderr and read a yes or no from stdin. `PAJA_TEST_ANSWER` stands in for the person
/// in tests (the smoke harness has no terminal).
fn confirmed() -> Result<bool> {
    // Like `progress`, never a panic on a closed stderr.
    {
        use std::io::Write as _;
        let mut err = std::io::stderr();
        let _ = write!(err, "Go ahead? [y/N] ");
        let _ = err.flush();
    }
    let mut answer = String::new();
    match std::env::var("PAJA_TEST_ANSWER") {
        Ok(forced) if cfg!(debug_assertions) => answer = forced,
        _ => {
            std::io::stdin().read_line(&mut answer)?;
        }
    }
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Give an item claimed for this pass back, so stopping before the build leaves the queue as it
/// was. A resumed item was active already and stays so.
fn unclaim(home: &Home, repo: &Path, item: &WorkItem, resumed: bool) -> Result<()> {
    if !resumed {
        jono::set_status(home, &item.repo, item.number, Status::Queued, None)?;
        // And off GitHub too, where other runners look: this runner's claim only, so another
        // runner's live claim on the item stands.
        jono::release_own_claim(home, repo, item.number);
    }
    Ok(())
}

/// With required CI the repository must have an active GitHub Actions workflow, or Paja would
/// wait for checks that never come. It checks that one exists, not that it runs on pull
/// requests: a repository whose workflows only run on push or on a schedule passes here and
/// then waits for CI that never reports.
fn require_workflow(repo: &Path) -> Result<()> {
    let list = run_gh(repo, &["workflow", "list"]).with_context(|| {
        format!(
            "listing the repository's workflows failed (a token without the `workflow` or \
             `repo` scope does this); fix the sign-in, or put `none` in {CI_FILE} on the default \
             branch to land on Vasara's checks and the reviewers alone"
        )
    })?;
    if list.is_empty() {
        bail!(
            "this repository has no active GitHub Actions workflow, so Paja would wait for CI \
             that never comes; add a workflow that runs the checks on pull requests, or put \
             `none` in {CI_FILE} on the default branch to land on Vasara's checks and the \
             reviewers alone"
        );
    }
    Ok(())
}

/// Take the next work item and run it to an outcome.
pub fn run_once(home: &Home, opts: RunOptions<'_>) -> Result<Pass> {
    let mut sandbox = false;
    let report = one_pass(home, opts, &mut sandbox)?;
    Ok(Pass { report, sandbox })
}

/// [`run_once`]'s pass, which sets `sandbox` once it has started one.
fn one_pass(home: &Home, opts: RunOptions<'_>, sandbox: &mut bool) -> Result<RunReport> {
    // The usage window is the account's: before it resets, nothing is claimed or resumed.
    if let Some(resets_at) = usage_pause(&home.events().read_all()?, crate::events::now_ms()) {
        progress(&format!(
            "Claude Code's usage limit hasn't reset; nothing was started. `paja run` can run \
             again at {}",
            format_utc(resets_at)
        ));
        return Ok(RunReport::Paused { resets_at });
    }
    let repo = std::path::absolute(opts.repo)?;
    if !repo.is_dir() {
        return Err(Mistake(format!(
            "{} is not a directory; pass --repo with a path inside the checkout",
            repo.display()
        ))
        .into());
    }
    // A repository whose configuration was tampered with gets no git, not even a fetch, until
    // a person says it was checked.
    if opts.tamper_checked {
        ahjo::clear_tampered(home, &repo)?;
    }
    let key = ahjo::ensure_untampered(home, &repo)?;
    // Once per pass: the default branch and the merge methods the repository allows.
    let target = RepoSettings::read(&repo)?;
    let branch = target.branch.as_str();
    let base = fetch_tip(&repo, branch).with_context(|| {
        format!(
            "fetching origin/{branch}; check the network and that `origin` is the GitHub \
             repository"
        )
    })?;
    // Before the queue is touched: a sandbox that can't start (or whose `.paja/setup` fails) is
    // the person's to fix, and must not park an item.
    let sbx = start_sandbox(home, &repo, &base)?;
    *sandbox = sbx.is_some();
    let _sandbox = crate::sandbox::enter(sbx);
    let labels =
        jono::Labels::parse(&read_from_base(&repo, &base, jono::LABELS_FILE)?.unwrap_or_default())?;
    let queue = jono::QueueRules::parse(
        &read_from_base(&repo, &base, jono::QUEUE_FILE)?.unwrap_or_default(),
    )?;
    progress(&format!(
        "checking the queue (issues labelled {}{})",
        labels.work,
        if queue.follow_ups {
            format!(", then {}", labels.follow_up)
        } else {
            String::new()
        }
    ));
    // The work items of this checkout's repository only: other repositories' items sharing the
    // home are never claimed.
    let name = if opts.sync {
        synced(jono::sync_with(home, &repo, Some(&labels), Some(&queue))?)
    } else {
        jono::repo_name(home, &repo)?
    };
    let config = read_config(&repo, &base, target)?;
    // A skipped `.paja/models` line is only a warning on stderr, which nobody reads at an
    // unattended runner, so Posti tells of each skipped role once.
    crate::posti::skipped_roles(
        home,
        &repo,
        &name,
        &config.target.branch,
        &config.models.skipped,
    );
    // A new item starts with a build, which a nearly used window would cut off, so it isn't
    // claimed at all (no status events) and the pass waits. A resumed item goes on to its round,
    // which makes the same check for each build.
    let nearly = window_nearly_used(
        &home.events().read_all()?,
        crate::events::now_ms(),
        config.models.window_percent(),
    );
    // Before the claim, so a repository without a workflow never has an item claimed and given
    // back on GitHub at every pass; only when something could be taken, so an idle pass never
    // calls GitHub for it; and before the build, because waiting for CI that will never run
    // fails after the whole build.
    // With the window nearly used, no new item is claimed, so only an item left active counts:
    // with none, the pass pauses below without calling GitHub, as an idle one does.
    let wanted = |n: u64| opts.item.is_none_or(|only| only == n);
    if config.ci == CiMode::Required
        && if nearly.is_some() {
            jono::left_active(home, &name)?.into_iter().any(wanted)
        } else {
            jono::runnable(home, &name)?
                .iter()
                .any(|i| wanted(i.number))
        }
    {
        require_workflow(&repo)?;
    }
    // The claim's lock is held until this pass is done with the item, however it ends.
    let Some(jono::RunClaim {
        item,
        lock: _working,
        resumed,
    }) = jono::claim_for_run_on(home, &repo, &name, opts.item, nearly.is_none())?
    else {
        if let Some(resets_at) = nearly {
            let ready = jono::runnable(home, &name)?
                .iter()
                .any(|i| wanted(i.number));
            if ready {
                progress(&format!(
                    "the usage window is nearly used up, so no new item was started; `paja run` \
                     can run again at {}",
                    format_utc(resets_at)
                ));
                return Ok(RunReport::Paused { resets_at });
            }
        }
        if let Some(number) = opts.item {
            return Err(Mistake(format!(
                "work item #{number} is not queued (or another run holds it); see `paja work \
                 list`"
            ))
            .into());
        }
        if let Some(waiting) = jono::waiting(home, &name)? {
            progress(&format!("nothing is ready: {waiting}"));
        }
        return Ok(RunReport::Idle);
    };
    if let Some(number) = opts.item {
        if let Some(other) = jono::left_active(home, &name)?
            .into_iter()
            .find(|&n| n != number)
        {
            progress(&format!(
                "#{other} was left active by an earlier run and is not resumed with --item; \
                 `paja run` without --item resumes it"
            ));
        }
    }
    summary(
        &item,
        &config.target,
        config.ci,
        crate::sandbox::active().is_some(),
    );
    let go = if opts.confirm { confirmed()? } else { true };
    if !go {
        unclaim(home, &repo, &item, resumed)?;
        return Err(Declined.into());
    }
    if resumed {
        progress(&format!(
            "resuming #{}: {} (an earlier run stopped before finishing it)",
            item.number, item.title
        ));
    } else {
        progress(&format!("picked #{}: {}", item.number, item.title));
    }
    // Anything that goes wrong from here is reported on the issue, not lost.
    let mut run = Unrecorded::default();
    match run_item(home, &repo, &base, &item, config, resumed, &mut run) {
        Ok(report) => Ok(report),
        // Ctrl-C: whatever failed after it (a step it cut short, or the stop itself) is not the
        // item's fault. Keep what the round spent, leave the item active and its environment in
        // place, and let the next run resume it. A tampered repository still stops as below.
        Err(err) if !is_tampered(&err) && crate::platform::interrupts() > 0 => {
            let rounds = run.round;
            if !run.steps.is_empty() {
                let note = format!("{err:#}");
                record_round(home, &item, rounds, "stopped", &note, &mut run.steps)?;
            }
            progress(&format!(
                "stopped; #{} stays active and the next `paja run` resumes it",
                item.number
            ));
            Ok(RunReport::Stopped {
                number: item.number,
                rounds,
            })
        }
        Err(err) => {
            let rounds = run.round;
            let question = format!(
                "Paja's run stopped with an error it can't fix by itself:\n\n```\n{err:#}\n```\n\n\
                 Fix the cause, then answer with what you fixed."
            );
            // Park the item first, so it never stays active if GitHub is the thing failing.
            jono::set_status(
                home,
                &item.repo,
                item.number,
                Status::Escalated,
                Some(&question),
            )?;
            // Recorded even with no step: the steps that ran were spent, and the item's last
            // verdict must be `error` (`paja usage` reports failed), not an earlier round's.
            let error = format!("{err:#}");
            record_round(home, &item, rounds, "error", &error, &mut run.steps)?;
            record(
                home,
                EV_ESCALATED,
                json!({ "repo": item.repo, "number": item.number, "rounds": rounds }),
            )?;
            if is_tampered(&err) {
                // gh would run git in the repository to find it, and removing the environment
                // runs git too; stop instead, loop included, and let the error tell the person.
                // The event keeps later runs out until a person has checked.
                record(
                    home,
                    EV_TAMPERED,
                    // `repo` is the git directory the tamper lock keys on; `work_repo` is the
                    // item's repository, so the queue can still be read there without gh.
                    json!({
                        "repo": key,
                        "work_repo": item.repo,
                        "number": item.number,
                        "error": format!("{err:#}")
                    }),
                )?;
                return Err(err);
            }
            let asked = comment(home, &repo, &item, &question);
            tell(home, &repo, &item);
            progress(&parked_line(
                &format!("#{} stopped on an error and was parked", item.number),
                item.number,
                asked.err(),
            ));
            free_after_error(home, &repo, &item);
            Ok(RunReport::Escalated {
                number: item.number,
                question,
                rounds,
            })
        }
    }
}

/// Sync Jono for the repository of the checkout `repo` as a run does before it claims: with the
/// labels and queue rules on the fetched default branch, never on a repository whose git
/// configuration was tampered with. Returns the repository's name (`owner/name`).
pub fn sync_queue(home: &Home, repo: &Path) -> Result<String> {
    let repo = std::path::absolute(repo)?;
    ahjo::ensure_untampered(home, &repo)?;
    let target = RepoSettings::read(&repo)?;
    let branch = target.branch.as_str();
    let base = fetch_tip(&repo, branch).with_context(|| {
        format!(
            "fetching origin/{branch}; check the network and that `origin` is the GitHub \
             repository"
        )
    })?;
    let labels =
        jono::Labels::parse(&read_from_base(&repo, &base, jono::LABELS_FILE)?.unwrap_or_default())?;
    let queue = jono::QueueRules::parse(
        &read_from_base(&repo, &base, jono::QUEUE_FILE)?.unwrap_or_default(),
    )?;
    Ok(synced(jono::sync_with(
        home,
        &repo,
        Some(&labels),
        Some(&queue),
    )?))
}

/// The repository a run's sync was of, after telling on stderr which environments of closed,
/// parked items it freed or kept and why, since a run prints no sync report.
fn synced(report: jono::SyncReport) -> String {
    for env in &report.freed {
        progress(&format!("freed {env}: its parked item's issue was closed"));
    }
    for kept in &report.kept {
        progress(&format!("kept {}: {}", kept.env, kept.why));
    }
    report.repo
}

/// For `paja doctor`: the deny-list hook a builder would run under passes its self-test in
/// `root`. Changes nothing but the hook's binary copy in the home, as a run does.
pub fn check_hook(home: &Home, root: &Path) -> Result<()> {
    let (_, stamp, _) = hook_settings(root, home, false, &[])?;
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(())
}

/// Where a run is, so an error can still record what the round spent: the round it is in and
/// the steps that ran in it but are not yet in a `run.round` event.
#[derive(Default)]
struct Unrecorded {
    round: u32,
    steps: Vec<StepUsage>,
}

/// Whether the repository has CI Paja waits for before landing (`.paja/ci` on the default
/// branch).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CiMode {
    /// Wait for the pull request's CI checks to pass (the default).
    Required,
    /// No CI: land on Vasara's checks and the reviewers alone.
    None,
}

/// How long a run waits for a pull request's CI, in minutes, when `.paja/ci` names no `wait`.
pub(crate) const CI_WAIT_MINUTES: u64 = 120;
/// The longest `wait` `.paja/ci` may name, in minutes: a day.
const CI_WAIT_MAX: u64 = 1440;

/// `.paja/ci`'s text: `required` (also when the file is missing or empty) or `none`, and with
/// `required` an optional `wait <minutes>` line, how long a pull request's CI may take before
/// the run stops waiting and asks the person. Returns the mode and the wait in minutes; an
/// error says what is wrong with the file, for the caller to name the branch it was read from.
pub(crate) fn parse_ci(text: &str) -> std::result::Result<(CiMode, u64), String> {
    let mut mode = None;
    let mut wait = None;
    for line in lines(text) {
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [w @ ("required" | "none")] => {
                if mode.is_some() {
                    return Err(format!(
                        "has `{line}` after an earlier `required` or `none` line; keep one"
                    ));
                }
                mode = Some(if *w == "none" {
                    CiMode::None
                } else {
                    CiMode::Required
                });
            }
            ["wait", minutes] => {
                if wait.is_some() {
                    return Err(format!(
                        "has `{line}` after an earlier `wait` line; keep one"
                    ));
                }
                match minutes.parse::<u64>() {
                    Ok(m) if (1..=CI_WAIT_MAX).contains(&m) => wait = Some(m),
                    _ => {
                        return Err(format!(
                            "has `{line}`, but the wait is a whole number of minutes from 1 to \
                             {CI_WAIT_MAX}"
                        ))
                    }
                }
            }
            _ => {
                return Err(format!(
                    "must hold `required` or `none`, and with `required` an optional \
                     `wait <minutes>` line; `{line}` is not one of them"
                ))
            }
        }
    }
    let mode = mode.unwrap_or(CiMode::Required);
    if mode == CiMode::None && wait.is_some() {
        return Err("says `none` and names a `wait`, which only `required` has".to_string());
    }
    Ok((mode, wait.unwrap_or(CI_WAIT_MINUTES)))
}

impl CiMode {
    /// [`parse_ci`] of the file as `branch` has it, with an error naming the branch.
    fn parse(text: Option<&str>, branch: &str) -> Result<(CiMode, u64)> {
        parse_ci(text.unwrap_or_default())
            .map_err(|why| anyhow!("{CI_FILE} on {branch} {why}; fix it on {branch}"))
    }
}

/// The repository's default branch and the merge methods it allows, from `gh repo view`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RepoSettings {
    branch: String,
    squash: bool,
    merge_commit: bool,
    rebase: bool,
}

impl RepoSettings {
    fn read(repo: &Path) -> Result<Self> {
        let gh = jono::gh_program();
        let out = run_gh(
            repo,
            &[
                "repo",
                "view",
                "--json",
                "defaultBranchRef,squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed",
            ],
        )?;
        let settings = Self::parse(&out);
        settings.with_context(|| {
            format!(
                "`{gh} repo view` gave no default branch: an empty repository has none until its \
                 first push, so push a first commit; otherwise check `gh auth status`"
            )
        })
    }

    /// A field `gh` leaves out is not taken to forbid anything: the branch is then `main` and
    /// squash merging is allowed.
    fn parse(json: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(json).ok()?;
        let branch = match v["defaultBranchRef"]["name"].as_str() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ if v.get("defaultBranchRef").is_none() => FALLBACK_BRANCH.to_string(),
            _ => return None,
        };
        let flag = |key: &str, default: bool| v[key].as_bool().unwrap_or(default);
        Some(RepoSettings {
            branch,
            squash: flag("squashMergeAllowed", true),
            merge_commit: flag("mergeCommitAllowed", false),
            rebase: flag("rebaseMergeAllowed", false),
        })
    }

    /// The `gh pr merge` flag to land with: squash, else rebase, else a merge commit.
    fn merge_flag(&self) -> Option<&'static str> {
        if self.squash {
            Some(SQUASH)
        } else if self.rebase {
            Some(REBASE)
        } else if self.merge_commit {
            Some(MERGE)
        } else {
            None
        }
    }
}

/// Vasara's check commands, the high-risk paths and the model per role, as the default branch
/// has them.
struct Config {
    ci: CiMode,
    /// How long to wait for a pull request's CI, in minutes.
    ci_wait: u64,
    checks: Vec<String>,
    high_risk: Vec<String>,
    models: Models,
    commands: crate::hook::Commands,
    target: RepoSettings,
}

/// Read and check the default branch's configuration once, before an item is claimed: a
/// mistake there is one fix on that branch, so it stops the pass instead of parking every item
/// in turn.
fn read_config(repo: &Path, base: &str, target: RepoSettings) -> Result<Config> {
    let branch = target.branch.as_str();
    let checks = read_from_base(repo, base, CHECKS_FILE)?.with_context(|| {
        format!("{CHECKS_FILE} is missing on {branch}; add the check commands there")
    })?;
    let checks: Vec<String> = lines(&checks);
    if checks.is_empty() {
        // An empty gate would pass everything; that is never what the branch meant.
        bail!("{CHECKS_FILE} on {branch} has no commands; add the check commands there");
    }
    let high_risk = lines(&read_from_base(repo, base, HIGH_RISK_FILE)?.unwrap_or_default());
    let models = Models::parse_on(
        &read_from_base(repo, base, MODELS_FILE)?.unwrap_or_default(),
        branch,
    )?;
    let commands = crate::hook::Commands::parse(
        read_from_base(repo, base, crate::hook::COMMANDS_FILE)?.as_deref(),
    )?;
    let (ci, ci_wait) = CiMode::parse(read_from_base(repo, base, CI_FILE)?.as_deref(), branch)?;
    Ok(Config {
        ci,
        ci_wait,
        checks,
        high_risk,
        models,
        commands,
        target,
    })
}

fn run_item(
    home: &Home,
    repo: &Path,
    base: &str,
    item: &WorkItem,
    config: Config,
    resumed: bool,
    run: &mut Unrecorded,
) -> Result<RunReport> {
    let Config {
        ci,
        ci_wait,
        checks,
        high_risk,
        models,
        commands,
        target,
    } = config;
    let (env, kept) = environment(home, repo, base, item)?;
    // The default branch the change is judged against: it moves forward when the branch is
    // brought up to date with it.
    let mut base = base.to_string();
    // An earlier run's builder session is resumed only for work still on its branch: an
    // environment made again from the default branch starts a fresh one.
    let mut carried = if kept && (resumed || !item.answers.is_empty()) {
        carried_session(home, item, &env.branch)?
    } else {
        None
    };
    record(
        home,
        EV_STARTED,
        json!({
            "repo": item.repo,
            "number": item.number,
            "env": env.name,
            "base": base,
            "backend": if crate::sandbox::active().is_some() { "sandbox" } else { "host" },
            "backend_why": crate::sandbox::backend_why().as_str(),
        }),
    )?;

    let mut feedback = String::new();
    let mut prior: Option<Prior> = None;
    let mut follow_ups: Vec<Finding> = Vec::new();
    // A resumed item goes on in the round that was cut off, with what that round was given.
    let mut start = 1;
    let mut reuse: Option<(String, String)> = None;
    // The findings of a review that approved the cut-off round's change, which goes on to land
    // without being reviewed again while it is still that change.
    let mut approved: Option<Approval> = None;
    if resumed && kept {
        let point = resume_point(&home.events().read_all()?, &item.repo, item.number);
        start = point.round;
        feedback = point.feedback;
        // With a prior, the earlier outside findings are its `follow_ups`; without, `noted`. They
        // go in `follow_ups` either way: a round that drops the prior and ends before a review
        // (a turn limit, say) records its `next.noted` from there.
        follow_ups = match &point.prior {
            Some(p) => p.follow_ups.clone(),
            None => point.noted,
        };
        prior = point.prior;
        reuse = point.checked;
        // The approval holds only for the very commit it approved, untouched since.
        if let (Some(approval), Some((commit, _))) = (point.approved, &reuse) {
            if approval.commit == *commit && untouched_at(&env, commit)? {
                follow_ups = approval.findings.clone();
                // The approval settled the blockers of the rounds before it: a fix after the
                // default branch is merged in reads the whole change, as at the turn to land.
                prior = None;
                approved = Some(approval);
            }
        }
        let was = match (&reuse, &prior, &approved) {
            (_, _, Some(approval)) => format!(
                "the review approved {}, which was waiting to land",
                short(&approval.reviewed)
            ),
            (Some((commit, _)), _, _) => {
                format!("the review of {} was interrupted", short(commit))
            }
            (None, Some(p), _) => format!(
                "the fix for the review of {} was interrupted",
                short(&p.reviewed)
            ),
            (None, None, _) if start > 1 => format!("{} before it", point.stopped_in),
            (None, None, _) => "the build was interrupted".to_string(),
        };
        progress(&format!(
            "resuming #{} in round {start}: {was}",
            item.number
        ));
    } else if resumed {
        // The branch is made again, so nothing an earlier round had carries over, but the
        // rounds it spent still count: the cap holds across runs that lose their environment.
        let point = resume_point(&home.events().read_all()?, &item.repo, item.number);
        start = point.round;
        // Past the cap, no round runs: the escalation quotes the last round's feedback.
        if start > MAX_ROUNDS {
            feedback = point.feedback;
        }
        if start > 1 {
            progress(&format!(
                "resuming #{} in round {start} on a new branch: the earlier rounds' work wasn't \
                 kept, but they count toward the cap of {MAX_ROUNDS} rounds",
                item.number
            ));
        }
    }
    // The builder's last result in this run: its session is resumed for a fix round, and its
    // running totals come off the next step's. Not kept across runs: an earlier run's session
    // comes from the event log instead (`carried`).
    let mut builder: Option<StepUsage> = None;
    // How often each kind of step hit its turn limit in this run: a second hit escalates.
    let (mut builder_limits, mut review_limits) = (0u32, 0u32);
    // Whether the last round ended on a red CI, whose feedback holds a CI log.
    let mut ci_red = false;
    progress(&format!("working in {}", env.path.display()));
    for round in start..=MAX_ROUNDS {
        run.round = round;
        stop_point()?;
        // The claim on GitHub goes stale after six hours without this. Round 1 of a new run
        // has just made it.
        if round > 1 || resumed {
            if let Err(err) = jono::renew_claim(home, repo, item.number) {
                progress(&format!(
                    "couldn't renew #{}'s claim on GitHub: {err:#}",
                    item.number
                ));
            }
        }
        let steps = &mut run.steps;
        steps.clear();
        ci_red = false;
        // Only the round that was cut off may reuse what it had: the build, and the checks if
        // nothing has changed since they passed.
        let reused = if round == start { reuse.take() } else { None };
        let approval = if round == start {
            approved.take()
        } else {
            None
        };
        let skip_build = match &reused {
            Some((commit, _)) => untouched_at(&env, commit)?,
            None => false,
        };
        // An approval holds only while the build is skipped. Merging the default branch in
        // below, which is Paja's own merge, is checked again as at the turn to land.
        let mut approval = approval.filter(|_| skip_build);
        if skip_build {
            progress(&format!(
                "round {round} of {MAX_ROUNDS}: the build is already committed; skipping it"
            ));
        } else {
            progress(&format!(
                "round {round} of {MAX_ROUNDS}: Claude is building the change (often 5 to 30 minutes)"
            ));
        }
        // A new branch starts with a build, whatever the round.
        let role = if round == 1 || (round == start && !kept) {
            "build"
        } else {
            "fix"
        };
        // A build the window can't finish buys nothing: keep the work and wait for the reset.
        // Reviews still start, so a built change isn't left unreviewed for a whole window.
        if !skip_build {
            if let Some(resets_at) = window_nearly_used(
                &home.events().read_all()?,
                crate::events::now_ms(),
                models.window_percent(),
            ) {
                let limit = UsageLimit {
                    result: None,
                    resets_at: Some(resets_at),
                    nearly: true,
                };
                return usage_stopped(home, repo, item, round, &limit, steps);
            }
        }
        // An earlier run's session is offered to the first build only.
        let earlier = if skip_build { None } else { carried.take() };
        if !skip_build {
            record_step(home, item, round, role, Some(&models), None, None)?;
        }
        let built = (!skip_build).then(|| {
            build(
                home,
                &env,
                item,
                &base,
                &feedback,
                &models,
                role,
                round,
                builder.as_ref(),
                earlier.as_ref(),
                steps,
                &commands.builder,
                &checks,
            )
        });
        let built = match built {
            None => None,
            Some(Ok(built)) => Some(built),
            Some(Err(err)) => {
                // The window, not the change: keep the worktree and resume after the reset.
                if let Some(limit) = usage_limit(&err) {
                    return usage_stopped(home, repo, item, round, &limit, steps);
                }
                let Some(limit) = turn_limit(&err) else {
                    return Err(err);
                };
                let raw = StepUsage::from_result("build", None, &models, role, &limit.0);
                // A resumed session's totals include its earlier rounds.
                let basis = builder
                    .as_ref()
                    .or(earlier.as_ref().and_then(|c| c.totals.as_ref()));
                let step = match basis {
                    Some(before)
                        if before.session_id.is_some() && before.session_id == raw.session_id =>
                    {
                        raw.since(before)
                    }
                    _ => raw.clone(),
                };
                builder = Some(raw);
                let note = format!(
                    "the builder hit the turn limit of {}",
                    models.max_turns(role)
                );
                builder_limits += 1;
                steps.push(step);
                if builder_limits > 1 {
                    record_round(home, item, round, "turn_limit", &note, steps)?;
                    let scene = Scene {
                        home,
                        repo,
                        env: &env,
                        base: &base,
                        item,
                        models: &models,
                    };
                    return turn_limit_twice(&scene, &note, round);
                }
                progress(&format!("{note}; asking it to finish smaller"));
                feedback = format!(
                    "Your last session stopped because {note}. Finish smaller: do only what the \
                     issue needs, avoid long explorations, commit what works, and keep outputs \
                     short (pipe long ones through `tail` or `grep`)."
                );
                // A merge it was finishing still has conflicts: name them, or the next round
                // starts without knowing. One resolved but not committed is committed with the
                // next round's leftovers.
                if let Some(merge) = unfinished_merge(&env, &target.branch)? {
                    feedback.push_str(&format!("\n\n{merge}"));
                }
                if let Some(p) = &prior {
                    feedback.push_str(&format!(
                        "\n\nThe review's blockers still need fixing too:\n\n{}",
                        p.blockers
                    ));
                }
                let next = Next {
                    feedback: &feedback,
                    prior: prior.as_ref(),
                    noted: &outside(&follow_ups),
                };
                record_round_next(home, item, round, "turn_limit", &note, steps, Some(next))?;
                continue;
            }
        };
        if let Some((step, raw, asked)) = built {
            builder = Some(raw);
            steps.push(step);
            // A question goes to the person at once: no checks run and no rounds are spent on it.
            if let Some(asked) = asked {
                progress("the builder asked for a decision");
                record_round(home, item, round, "question", &asked, steps)?;
                let question =
                    format!("Paja's builder needs a decision before it can go on:\n\n{asked}");
                let scene = Scene {
                    home,
                    repo,
                    env: &env,
                    base: &base,
                    item,
                    models: &models,
                };
                return settle(&scene, &question, "", "", round);
            }
        }
        // A merge Paja handed over must be finished, not committed with its conflicts. It stays
        // in progress, so the next round sees the files that still conflict.
        if unfinished_merge(&env, &target.branch)?.is_some() {
            // The next round starts from the conflicts again: restart the merge, so the files
            // hold markers for the builder to resolve.
            git(&env.path, &["merge", "--abort"])?;
            feedback = match catch_up(repo, &env, &target.branch)? {
                CatchUp::Conflict(files) => format!(
                    "The merge of {} isn't finished: these files still have conflicts: {}. \
                     Paja started the merge again, so they hold conflict markers. Resolve each \
                     one, keeping what both sides meant, then `git add` the files and `git \
                     commit -m '<message>'` to finish the merge.",
                    target.branch,
                    files.join(", ")
                ),
                // The default branch moved so the merge is clean now, or the branch already
                // holds it: nothing is left to resolve.
                _ => format!(
                    "The merge of {} wasn't finished and Paja has started it again. Make the \
                     change and commit it.",
                    target.branch
                ),
            };
            if let Some(p) = &prior {
                feedback.push_str(&format!(
                    "\n\nThe review's blockers still need fixing too:\n\n{}",
                    p.blockers
                ));
            }
            progress("the builder left the merge unfinished; asking again");
            let next = Next {
                feedback: &feedback,
                prior: prior.as_ref(),
                noted: &outside(&follow_ups),
            };
            record_round_next(home, item, round, "conflict", &feedback, steps, Some(next))?;
            continue;
        }
        commit_leftovers(&env, item)?;
        if git(
            &env.path,
            &["rev-list", "--count", &format!("{base}..HEAD")],
        )? == "0"
        {
            feedback = "The previous round made no commits. Make the change and commit it.".into();
            progress("the builder committed nothing; asking again");
            let next = Next {
                feedback: &feedback,
                prior: prior.as_ref(),
                noted: &outside(&follow_ups),
            };
            record_round_next(home, item, round, "no_change", &feedback, steps, Some(next))?;
            continue;
        }
        // A round after a review must commit a fix; an empty fix is never reviewed.
        if let Some(prior) = &prior {
            if git(&env.path, &["rev-parse", "HEAD"])? == prior.reviewed {
                feedback = format!(
                    "The previous round made no new commits. Fix these blockers and commit:\n\n{}",
                    prior.blockers
                );
                progress("the builder committed no fix; asking again");
                let next = Next {
                    feedback: &feedback,
                    prior: Some(prior),
                    noted: &outside(&follow_ups),
                };
                record_round_next(home, item, round, "no_change", &feedback, steps, Some(next))?;
                continue;
            }
        }
        // Judge the change as it would land: on the latest default branch.
        match catch_up(repo, &env, &target.branch)? {
            // The builder may have finished a merge Paja handed over: diffs start from there.
            CatchUp::Current(tip) => base = tip,
            CatchUp::Merged(tip) => {
                progress(&format!(
                    "{} moved on; merged it into the branch",
                    target.branch
                ));
                base = tip;
            }
            CatchUp::Conflict(files) => {
                let branch = &target.branch;
                // An approved change from the run that stopped meets main as at the turn to land.
                feedback = conflict_feedback(branch, approval.is_some(), &files, prior.as_ref());
                progress(&format!(
                    "the branch conflicts with {branch}; sending the merge to the builder"
                ));
                let next = Next {
                    feedback: &feedback,
                    prior: prior.as_ref(),
                    noted: &outside(&follow_ups),
                };
                let detail = files.join(", ");
                record_round_next(home, item, round, "conflict", &detail, steps, Some(next))?;
                continue;
            }
        }
        let checked = git(&env.path, &["rev-parse", "HEAD"])?;
        // The checks passed on this very commit, against this very default branch tip, in the
        // run that stopped, and nothing in the worktree has changed since: not run again.
        let skip_checks = match &reused {
            Some((commit, at)) => {
                *commit == checked && *at == base && untouched_at(&env, &checked)?
            }
            None => false,
        };
        if skip_checks {
            progress("Vasara's checks already passed on this commit; skipping them");
        }
        if !skip_checks {
            let failed = vasara(home, &env, item, round, &checks, &checked)?;
            if let Some(failure) = failed {
                feedback = if approval.is_some() {
                    format!(
                        "{} moved on after the review, and with it merged in Vasara's checks \
                         fail. Fix the cause, not the check:\n\n{failure}",
                        target.branch
                    )
                } else {
                    format!("Vasara's checks failed. Fix the cause, not the check:\n\n{failure}")
                };
                if let Some(prior) = &prior {
                    feedback.push_str(&format!(
                        "\n\nThe review's blockers still need fixing too:\n\n{}",
                        prior.blockers
                    ));
                }
                progress("the checks failed; sending the failure back to the builder");
                let next = Next {
                    feedback: &feedback,
                    prior: prior.as_ref(),
                    noted: &outside(&follow_ups),
                };
                record_round_next(
                    home,
                    item,
                    round,
                    "checks_failed",
                    &failure,
                    steps,
                    Some(next),
                )?;
                continue;
            }
            record_checked(
                home,
                item,
                round,
                &checked,
                &base,
                checks.len(),
                approval.is_some(),
            )?;
        }
        // --no-renames lists both sides of a rename, so moving a high-risk file keeps it high risk.
        let changed = git(
            &env.path,
            &[
                "diff",
                "--name-only",
                "--no-renames",
                // NUL-separated paths are never quoted, whatever characters they hold.
                "-z",
                &format!("{base}...HEAD"),
            ],
        )?;
        let paths: Vec<&str> = changed
            .split('\0')
            .filter(|path| !path.is_empty())
            .collect();
        // What the change touches, which a pool compares with the files it admitted it for.
        record(
            home,
            EV_CHANGED,
            json!({ "repo": item.repo, "number": item.number, "round": round, "paths": paths }),
        )?;
        let risky = paths.iter().any(|path| is_high_risk(path, &high_risk));
        let reviewers = if risky { 2 } else { 1 };
        // `.paja/high-risk` on the default branch may have changed since the approval.
        if let Some(fewer) = approval.as_ref().filter(|a| a.reviewers < reviewers) {
            progress(&format!(
                "the change is high risk now and needs {reviewers} reviewers, but {} approved it; \
                 reviewing it again",
                fewer.reviewers
            ));
            approval = None;
        }
        // A review that reads the whole change (high risk, a fix that isn't a plain diff, or the
        // round after a conflict or failed checks at the turn to land) still hears what earlier
        // reviews noted outside the change, so those findings aren't lost.
        let noted: Vec<Finding> = match &prior {
            Some(p) => p.follow_ups.clone(),
            None => outside(&follow_ups),
        };
        // A fix that merged or rewrote history isn't a plain diff on what was reviewed: read it all.
        if let Some(p) = &prior {
            let plain = git(
                &env.path,
                &["merge-base", "--is-ancestor", &p.reviewed, "HEAD"],
            )
            .is_ok()
                && git(
                    &env.path,
                    &["rev-list", "--merges", &format!("{}..HEAD", p.reviewed)],
                )?
                .is_empty();
            if !plain {
                prior = None;
            }
        }
        let mut reviews: Vec<Review> = Vec::new();
        let mut limited = None;
        // A reviewer at its turn limit gets one more, focused try in the round, and the other
        // reviewer's finished verdict is kept: only the capped review runs again.
        let mut retried = false;
        let mut i = 0;
        progress("Vasara's checks passed");
        stop_point()?;
        let approved_before = approval.is_some();
        if approved_before {
            progress("the review already approved this change; skipping it");
        }
        while i < reviewers && !approved_before {
            // High-risk reviewers read the whole change every round: the two must agree on all
            // of it, not only on the fix.
            progress(&format!(
                "reviewer {} of {reviewers} is reading the change{}",
                i + 1,
                if risky {
                    " (high risk: two must agree)"
                } else {
                    ""
                }
            ));
            let role = review_role(risky, i, prior.is_some());
            let reviewer = i as u32 + 1;
            record_step(
                home,
                item,
                round,
                role,
                Some(&models),
                Some(reviewer),
                Some(reviewers as u32),
            )?;
            let reviewed = review(
                home,
                &env,
                item,
                &base,
                &checked,
                prior.as_ref().filter(|_| !risky),
                &noted,
                reviewer,
                &models,
                role,
                retried,
                &checks,
                &commands.reviewer,
            );
            match reviewed {
                Ok((review, used)) => {
                    steps.push(used);
                    record_reviewed(home, item, round, reviewer, &review.verdict)?;
                    reviews.push(review);
                    i += 1;
                }
                // The review already put the worktree back. Its spend still counts.
                Err(err) => match turn_limit(&err) {
                    Some(limit) => {
                        let used = StepUsage::from_result(
                            "review",
                            Some(reviewer),
                            &models,
                            role,
                            &limit.0,
                        );
                        steps.push(used);
                        let note = format!(
                            "reviewer {reviewer} hit the turn limit of {}",
                            models.max_turns(role)
                        );
                        if !retried {
                            retried = true;
                            progress(&format!("{note}; asking it again to stay focused"));
                            continue;
                        }
                        limited = Some(note);
                        break;
                    }
                    None => {
                        if let Some(limit) = usage_limit(&err) {
                            if let Some(result) = &limit.result {
                                steps.push(StepUsage::from_result(
                                    "review",
                                    Some(reviewer),
                                    &models,
                                    role,
                                    result,
                                ));
                            }
                            return usage_stopped(home, repo, item, round, &limit, steps);
                        }
                        return Err(err);
                    }
                },
            }
        }
        if let Some(note) = limited {
            review_limits += 1;
            if review_limits > 1 {
                record_round(home, item, round, "turn_limit", &note, steps)?;
                let scene = Scene {
                    home,
                    repo,
                    env: &env,
                    base: &base,
                    item,
                    models: &models,
                };
                return turn_limit_twice(&scene, &note, round);
            }
            progress(&format!(
                "{note}; asking the builder to make the change easier to review"
            ));
            feedback = format!(
                "The change was not reviewed: {note} reading it. Make it smaller and easier to \
                 review: drop anything the issue doesn't need, and commit."
            );
            if let Some(p) = &prior {
                feedback.push_str(&format!(
                    "\n\nThe review's blockers still need fixing too:\n\n{}",
                    p.blockers
                ));
            }
            let next = Next {
                feedback: &feedback,
                prior: prior.as_ref(),
                noted: &outside(&follow_ups),
            };
            record_round_next(home, item, round, "turn_limit", &note, steps, Some(next))?;
            continue;
        }
        // Only the latest review's findings count: an earlier round's may be about code the fix
        // has since changed. An approval from the run that stopped brings its own.
        match approval {
            Some(approval) => follow_ups = approval.findings,
            None => {
                follow_ups.clear();
                for review in &reviews {
                    for finding in &review.follow_ups {
                        if !finding.text.trim().is_empty() && !follow_ups.contains(finding) {
                            follow_ups.push(finding.clone());
                        }
                    }
                }
            }
        }
        // No reviews ran after an approval: it stands for every reviewer's `land`.
        let verdicts: Vec<&str> = reviews.iter().map(|r| r.verdict.as_str()).collect();
        if !approved_before {
            progress(&format!("review verdict: {}", verdicts.join(", ")));
        }
        // Every finding in the change is fixed in it: a clean `land` with some still open goes
        // back to the builder, until the last round, which lands with them listed.
        let all_land = verdicts.iter().all(|v| *v == "land");
        let fixes = to_fix(&follow_ups);
        if !approved_before && all_land && !fixes.is_empty() && round < MAX_ROUNDS {
            let list = fixes
                .iter()
                .map(|f| f.shown())
                .collect::<Vec<_>>()
                .join("\n\n");
            progress("review left findings; sending them back to the builder");
            feedback = format!(
                "The reviewers would land this once these findings are fixed. Fix each one, or \
                 leave one only when it is plainly wrong, and commit:\n\n{list}"
            );
            prior = Some(Prior {
                reviewed: checked,
                blockers: list,
                follow_ups: outside(&follow_ups),
            });
            let next = Next {
                feedback: &feedback,
                prior: prior.as_ref(),
                noted: &outside(&follow_ups),
            };
            record_round_next(
                home,
                item,
                round,
                &verdicts.join(","),
                &summaries(&reviews),
                steps,
                Some(next),
            )?;
            continue;
        }
        // A round that goes on is recorded below, with what the next one starts from.
        let ends =
            all_land || verdicts.contains(&"escalate") || (risky && verdicts.contains(&"land"));
        if ends && !approved_before {
            // The approved review's findings go in the log too: a run stopped at the turn to
            // land, or in the CI wait, resumes with them and without reviewing again.
            record_round_noted(
                home,
                item,
                round,
                &verdicts.join(","),
                &summaries(&reviews),
                steps,
                &outside(&follow_ups),
                all_land.then(|| Approval {
                    reviewed: checked.clone(),
                    commit: checked.clone(),
                    reviewers,
                    findings: follow_ups.clone(),
                }),
            )?;
        }

        if let Some(esc) = reviews.iter().find(|r| r.verdict == "escalate") {
            let scene = Scene {
                home,
                repo,
                env: &env,
                base: &base,
                item,
                models: &models,
            };
            return settle(&scene, &esc.question, "", &summaries(&reviews), round);
        }
        if verdicts.iter().all(|v| *v == "land") {
            // One change lands at a time per repository, each on the latest default branch: a
            // lane that waited while another landed merges that landing in and is checked again.
            let landing = land_turn(home, item, round)?;
            let mut judged = checked.clone();
            match catch_up(repo, &env, &target.branch)? {
                CatchUp::Current(_) => {}
                CatchUp::Merged(tip) => {
                    progress(&format!(
                        "{} moved on while this change waited to land; merged it in, checking again",
                        target.branch
                    ));
                    base = tip;
                    judged = git(&env.path, &["rev-parse", "HEAD"])?;
                    if let Some(failure) = vasara(home, &env, item, round, &checks, &judged)? {
                        drop(landing);
                        feedback = format!(
                            "{} moved on after the review, and with it merged in Vasara's checks \
                             fail. Fix the cause, not the check:\n\n{failure}",
                            target.branch
                        );
                        // The next review reads all of it: the fix comes after a merge.
                        prior = None;
                        progress("the checks failed on the latest default branch; sending the failure back to the builder");
                        let next = Next {
                            feedback: &feedback,
                            prior: None,
                            noted: &outside(&follow_ups),
                        };
                        record_round_next(
                            home,
                            item,
                            round,
                            "checks_failed",
                            &failure,
                            steps,
                            Some(next),
                        )?;
                        continue;
                    }
                    record_checked(home, item, round, &judged, &base, checks.len(), true)?;
                }
                CatchUp::Conflict(files) => {
                    drop(landing);
                    let branch = &target.branch;
                    feedback = conflict_feedback(branch, true, &files, None);
                    prior = None;
                    progress(&format!(
                        "{branch} moved on after the review and conflicts with the branch; sending the merge to the builder"
                    ));
                    let next = Next {
                        feedback: &feedback,
                        prior: None,
                        noted: &outside(&follow_ups),
                    };
                    let detail = files.join(", ");
                    record_round_next(home, item, round, "conflict", &detail, steps, Some(next))?;
                    continue;
                }
            }
            // What the change lands without: findings outside it, and at the round limit the
            // ones still open. No issue is filed; the pull request lists them.
            let left = unsettled(home, repo, &follow_ups);
            let notes: Vec<String> = left.iter().map(|(text, _)| text.clone()).collect();
            let landed = land(
                home, repo, &env, item, round, &judged, ci, ci_wait, &target, &notes,
            );
            drop(landing);
            let report = match landed {
                Ok(Landing::Retry(why)) => {
                    progress("CI failed; sending the failure back to the builder");
                    ci_red = true;
                    feedback = why.clone();
                    // Every finding in the change was fixed before landing, so only those outside
                    // it are left for the re-reviewer to keep or drop.
                    prior = Some(Prior {
                        reviewed: judged,
                        blockers: why.clone(),
                        follow_ups: outside(&follow_ups),
                    });
                    let next = Next {
                        feedback: &feedback,
                        prior: prior.as_ref(),
                        noted: &outside(&follow_ups),
                    };
                    record_round_next(home, item, round, "ci_failed", &why, steps, Some(next))?;
                    // At the round limit no round follows and the run escalates: keep what it
                    // would have landed without in the log, after the round is recorded so a
                    // resumed run does not log them twice.
                    if round == MAX_ROUNDS {
                        for (text, _) in &left {
                            record_follow_up(
                                home,
                                item,
                                text,
                                None,
                                "not filed: the change did not land",
                            );
                        }
                    }
                    continue;
                }
                Ok(Landing::Done(report)) => Ok(report),
                Err(err) => Err(err),
            };
            match &report {
                Ok(RunReport::Landed { pr, .. }) => {
                    for (text, why) in &left {
                        record_follow_up(home, item, text, None, why);
                    }
                    // Findings in the change left at the round limit are for a person to read:
                    // Posti tells them, since no issue holds them.
                    let open = left.iter().filter(|(_, why)| *why == ROUND_LIMIT).count();
                    if open > 0 {
                        let notice = crate::posti::Notice::left_findings(
                            &item.repo,
                            item.number,
                            &item.title,
                            pr,
                            open,
                        );
                        crate::posti::send(home, repo, &notice);
                    }
                }
                // Stopped (`stop now` in the CI wait): the run that resumes the item records
                // them, once.
                Err(_) if crate::platform::interrupts() > 0 => {}
                // Not landed after all: keep the findings in the log, for later.
                _ => {
                    for (text, _) in &left {
                        record_follow_up(
                            home,
                            item,
                            text,
                            None,
                            "not filed: the change did not land",
                        );
                    }
                }
            }
            return report;
        }
        if risky && verdicts.contains(&"land") {
            // High-risk reviewers must agree; a split is a question, not another round.
            let said = summaries(&reviews);
            let question =
                format!("The two reviewers disagree on this high-risk change.\n\n{said}");
            let scene = Scene {
                home,
                repo,
                env: &env,
                base: &base,
                item,
                models: &models,
            };
            return settle(&scene, &question, &said, "", round);
        }
        // A `changes` verdict without comments still names something: its summary.
        let blockers = reviews
            .iter()
            .filter(|r| r.verdict == "changes")
            .map(|r| {
                if r.comments.trim().is_empty() {
                    r.summary.as_str()
                } else {
                    r.comments.as_str()
                }
            })
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        feedback = format!("A reviewer asked for changes:\n\n{blockers}");
        // Every other finding in the change is fixed in it too. A nit that needs more than a
        // rename or a comment may be left.
        let others: Vec<String> = to_fix(&follow_ups).iter().map(|f| f.shown()).collect();
        if !others.is_empty() {
            feedback.push_str(&format!(
                "\n\nThe reviewers also noted these findings. Fix each one; a nit that needs \
                 more than a rename or a comment may be left:\n\n{}",
                others.join("\n\n")
            ));
        }
        // Without named blockers there is nothing to narrow the next review to. The other
        // findings in the change join the fixes it checks; only those outside it are kept or
        // dropped.
        prior = (!blockers.is_empty()).then(|| {
            let mut asked = blockers;
            if !others.is_empty() {
                asked.push_str(&format!("\n\n{}", others.join("\n\n")));
            }
            Prior {
                reviewed: checked,
                blockers: asked,
                follow_ups: outside(&follow_ups),
            }
        });
        let next = Next {
            feedback: &feedback,
            prior: prior.as_ref(),
            noted: &outside(&follow_ups),
        };
        record_round_next(
            home,
            item,
            round,
            &verdicts.join(","),
            &summaries(&reviews),
            steps,
            Some(next),
        )?;
    }
    // A CI log can hold anything the build printed, and the question goes to a public issue.
    let (reason, feedback) = if ci_red {
        (
            "CI still failed",
            follow_ups::redact_for_comment(&feedback, repo, home.root(), follow_ups::QUOTED_CHARS),
        )
    } else {
        ("the change still isn't ready", feedback)
    };
    let question = format!(
        "Paja tried {MAX_ROUNDS} rounds and {reason}. Last feedback:\n\n{feedback}\n\n\
         Answer with what should change, or what is out of scope."
    );
    let scene = Scene {
        home,
        repo,
        env: &env,
        base: &base,
        item,
        models: &models,
    };
    settle(&scene, &question, &feedback, "", MAX_ROUNDS)
}

/// The latest reset time any usage-limit stop recorded, in milliseconds since the epoch, when it
/// is still ahead of `now_ms`. The window is the account's, so it holds every item and repository.
pub fn usage_pause(events: &[crate::events::Event], now_ms: u64) -> Option<u64> {
    events
        .iter()
        .filter(|e| e.kind == EV_ROUND && e.data["verdict"] == "usage_limit")
        .filter_map(|e| e.data["resets_at"].as_u64())
        .max()
        .filter(|&resets| resets > now_ms)
}

/// Whether the item's last round, since it was queued, claimed or answered, already stopped on
/// the usage limit without a reset time: a second such stop in a row parks it.
fn limit_without_reset(events: &[crate::events::Event], repo: &str, number: u64) -> bool {
    let mut last = false;
    for event in jono::item_events(events, repo, number) {
        match event.kind.as_str() {
            EV_ROUND => {
                last = event.data["verdict"] == "usage_limit" && event.data["resets_at"].is_null()
            }
            jono::EV_STATUS | jono::EV_ANSWERED | jono::EV_REOPENED | EV_LANDED | EV_ESCALATED => {
                last = false
            }
            _ => {}
        }
    }
    last
}

/// A step stopped at the usage limit. Record the round as `usage_limit` with what it spent (and
/// `resets_at`, when known) and leave the item active with its worktree in place, so the next
/// `paja run` resumes it after the window resets. Without a reset time the next run may simply
/// try again, but a second stop in a row parks the item, so a broken sign-in or account can't loop.
fn usage_stopped(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    round: u32,
    limit: &UsageLimit,
    steps: &mut Vec<StepUsage>,
) -> Result<RunReport> {
    let again = limit.resets_at.is_none()
        && limit_without_reset(&home.events().read_all()?, &item.repo, item.number);
    let note = match limit.resets_at {
        Some(ms) if limit.nearly => format!(
            "the usage window is nearly used up, so no build was started; it resets at {}",
            format_utc(ms)
        ),
        Some(ms) => format!(
            "Claude Code hit its usage limit; the window resets at {}",
            format_utc(ms)
        ),
        None => "Claude Code hit its usage limit and gave no reset time".to_string(),
    };
    let steps = std::mem::take(steps);
    record(
        home,
        EV_ROUND,
        json!({
            "repo": item.repo,
            "number": item.number,
            "round": round,
            "verdict": "usage_limit",
            "detail": note,
            "resets_at": limit.resets_at,
            "steps": steps,
        }),
    )?;
    if again {
        let question =
            "Paja's run hit Claude Code's usage or rate limit twice in a row and neither \
                        time was told when the window resets, so it stopped instead of trying \
                        again. Wait until it resets (and check that `claude` is signed in to the \
                        right account), then answer the item to run it again.";
        // Unlike `escalated`, keep the worktree: the retried round's uncommitted edits are
        // still in it, and the answer resumes from there.
        let failed = land::escalate(home, repo, item, question, round)?;
        progress(&land::parked_line(
            &format!("#{} needs a decision", item.number),
            item.number,
            failed,
        ));
        return Ok(RunReport::Escalated {
            number: item.number,
            question: question.to_string(),
            rounds: round,
        });
    }
    progress(&match limit.resets_at {
        Some(ms) => format!(
            "{note}; #{} stays active and `paja run` resumes it from {}",
            item.number,
            format_utc(ms)
        ),
        None => format!(
            "{note}; #{} stays active and the next `paja run` tries it again",
            item.number
        ),
    });
    Ok(RunReport::UsageLimit {
        number: item.number,
        rounds: round,
        resets_at: limit.resets_at,
    })
}

/// A time in milliseconds since the epoch as `2026-11-27 18:30 UTC`.
pub fn format_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a calendar date (proleptic Gregorian).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// The same kind of step hit its turn limit a second time in this run: ask a person rather
/// than spend another round.
fn turn_limit_twice(scene: &Scene, note: &str, round: u32) -> Result<RunReport> {
    let question = format!(
        "In this run {note}, the second time that step stopped at its turn limit. The issue may \
         be too big for one change: split it into smaller issues, or raise the limit with a \
         `max-turns` line in `{MODELS_FILE}` on the default branch, then answer with what you changed."
    );
    settle(scene, &question, "", "", round)
}

/// The environment an earlier run made for the item in `repo`, if there is one: the one named
/// by [`WorkItem::env_name`], else the `work-<n>` one that releases before repositories were
/// recorded made, so an item started before an upgrade keeps its branch and session.
fn existing_environment(home: &Home, repo: &Path, item: &WorkItem) -> Result<Option<Env>> {
    let name = item.env_name();
    // The name holds only a short hash of the repository, so two repositories could in
    // principle share it; never run one's item in the other's worktree.
    let same = ahjo::same_repo;
    if let Ok(env) = ahjo::get(home, &name) {
        if !same(&env.repo, repo) {
            bail!(
                "environment {name} belongs to the repository at {}, not {}; two repositories \
                 got the same environment name, so remove it with `paja env remove {name}` \
                 if it is stale",
                env.repo.display(),
                repo.display()
            );
        }
        return Ok(Some(env));
    }
    // `work-<n>` carries no repository, so it is this item's only if it was made in `repo`.
    let legacy = format!("work-{}", item.number);
    Ok(ahjo::get(home, &legacy)
        .ok()
        .filter(|env| same(&env.repo, repo)))
}

/// Frees the environment a run that stopped on an error used: the item's, or the `work-<n>` it
/// resumed. One whose name another repository's environment holds is that repository's, and
/// stays. A `work-<n>` may hold uncommitted edits an older release's park kept (a builder's
/// question, a usage limit), so it is removed only when clean and on its branch (commits made on
/// a detached HEAD would be lost), and kept with a line saying why otherwise. Its branch stays
/// either way, for the next run to resume.
fn free_after_error(home: &Home, repo: &Path, item: &WorkItem) {
    match existing_environment(home, repo, item) {
        Ok(Some(env)) => {
            let legacy = env.name == format!("work-{}", item.number);
            let fix = format!(
                "commit or clear the changes there, or run `paja env remove {} --force` to \
                 discard them",
                env.name
            );
            if legacy {
                let head = git(&env.path, &["symbolic-ref", "-q", "HEAD"]);
                let wanted = format!("refs/heads/{}", env.branch);
                if !head.as_deref().is_ok_and(|head| head.trim() == wanted) {
                    progress(&format!(
                        "kept the environment {}: its worktree {} isn't on {}; {fix}",
                        env.name,
                        env.path.display(),
                        env.branch
                    ));
                    return;
                }
            }
            if let Err(err) = ahjo::remove(home, &env.name, !legacy) {
                progress(&format!(
                    "kept the environment {}: {err:#}; {fix}",
                    env.name
                ));
            }
        }
        Ok(None) => {}
        Err(err) => progress(&format!("kept the environment: {err:#}")),
    }
}

/// A fresh environment for the item, branched from the base. A branch kept from an earlier
/// run of the same item is resumed, so its work isn't thrown away: the item's own, else the
/// `paja/work-<n>` of a `work-<n>` environment an older release made and something removed
/// since (a sync freeing a closed item, `paja env remove`), which is renamed to the item's own
/// unless [`legacy_left_behind`] says it isn't the item's.
/// The flag says whether the branch (and so the work on it) was kept from an earlier run, as
/// opposed to made again from the base.
fn environment(home: &Home, repo: &Path, base: &str, item: &WorkItem) -> Result<(Env, bool)> {
    let name = item.env_name();
    if let Some(env) = existing_environment(home, repo, item)? {
        return Ok((env, true));
    }
    let kept = |branch: &str| {
        git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .is_ok()
    };
    let own = format!("paja/{name}");
    let legacy = format!("paja/work-{}", item.number);
    // The old branch becomes the item's own, so it is carried over once: a later run (after a
    // landing deleted the own branch, say) never finds work that already landed there. The
    // builder session of the `work-<n>` run isn't resumed: it is recorded on the old branch, and
    // it worked in the old worktree's path anyway.
    let resume = kept(&own)
        || (kept(&legacy)
            && match legacy_left_behind(home, repo, item, &legacy)? {
                None => {
                    git(repo, &["branch", "-m", &legacy, &own])
                        .with_context(|| format!("renaming {legacy} to {own} to resume it"))?;
                    true
                }
                Some(why) => {
                    progress(&format!(
                        "not resuming {legacy}: {why}; starting fresh (the branch is kept: merge \
                         it into {own} if it holds this item's work)"
                    ));
                    false
                }
            });
    let base = if resume { None } else { Some(base) };
    let env = ahjo::create(
        home,
        CreateOptions {
            name: &name,
            repo,
            base,
        },
    )?;
    Ok((env, resume))
}

/// Why the `paja/work-<n>` branch an older release left in `repo` isn't the item's to carry
/// over, or None when it is. It is the item's only when the log shows it: a run of the item
/// started in the `work-<n>` environment, no run of the item landed after that, and no other
/// item's run started in a `work-<n>` since (see [`jono::ran_in_work_n`]). It is also left when a
/// `work-<n>` environment still exists (made from another checkout, or from a repository that
/// has moved, as the caller already checked) or a worktree has the branch checked out (a
/// person, or a worktree no environment records).
fn legacy_left_behind(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    legacy: &str,
) -> Result<Option<String>> {
    let number = item.number;
    let plain = format!("work-{number}");
    if let Ok(env) = ahjo::get(home, &plain) {
        return Ok(Some(format!(
            "environment {plain} belongs to the repository at {}, not {}",
            env.repo.display(),
            repo.display()
        )));
    }
    let worktrees = git(repo, &["worktree", "list", "--porcelain"])?;
    let wanted = format!("branch refs/heads/{legacy}");
    if worktrees.lines().any(|line| line.trim() == wanted) {
        return Ok(Some(format!(
            "a worktree has it checked out (see `git -C {repo} worktree list`; if its folder is \
             gone, `git -C {repo} worktree prune` frees the branch)",
            repo = repo.display()
        )));
    }
    let events = home.events().read_all()?;
    if !jono::ran_in_work_n(&events, &item.repo, number) {
        return Ok(Some(format!(
            "the log shows no unlanded run of #{number} in {plain} as the latest run there"
        )));
    }
    Ok(None)
}

/// A builder session of an earlier run of the item, which the next run may resume.
struct Carried {
    session_id: String,
    /// The short prompt the resumed session gets instead of the whole issue.
    prompt: String,
    /// What the recorded steps of that session add up to, to take off its running totals.
    totals: Option<StepUsage>,
}

/// Whether `id` is safe to hand to `claude --resume` (and to a sandbox shell): a UUID-like word.
fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A new version 4 UUID, for a session Mestari names itself.
fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    for chunk in bytes.chunks_mut(8) {
        // `RandomState` is seeded by the operating system; time, process and a counter keep two
        // ids apart even if it were not.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(crate::events::now_ms());
        hasher.write_u32(std::process::id());
        hasher.write_u64(COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The builder session an earlier run left for the item on `branch`, from the event log: the
/// last one started, unless its work was discarded or the item landed since. `escalated` says
/// the run after it ended by asking a person.
fn earlier_session(
    events: &[crate::events::Event],
    repo: &str,
    number: u64,
    branch: &str,
) -> Option<(String, bool)> {
    let mut last: Option<(String, bool)> = None;
    for event in jono::item_events(events, repo, number) {
        match event.kind.as_str() {
            EV_STEP_STARTED => {
                let id = event.data["session_id"]
                    .as_str()
                    .filter(|id| is_session_id(id));
                last = match (id, event.data["branch"].as_str()) {
                    (Some(id), Some(b)) if b == branch => Some((id.to_string(), false)),
                    _ => None,
                };
            }
            EV_STEP_DISCARDED => {
                if last
                    .as_ref()
                    .is_some_and(|(id, _)| event.data["session_id"].as_str() == Some(id))
                {
                    last = None;
                }
            }
            EV_ESCALATED => {
                if let Some((_, escalated)) = &mut last {
                    *escalated = true;
                }
            }
            EV_LANDED => last = None,
            _ => {}
        }
    }
    last
}

/// The earlier builder session to resume in this run, with the short prompt it gets. Nothing
/// when the log holds none, or when the sandbox lost its transcript.
fn carried_session(home: &Home, item: &WorkItem, branch: &str) -> Result<Option<Carried>> {
    let events = home.events().read_all()?;
    let Some((session_id, escalated)) = earlier_session(&events, &item.repo, item.number, branch)
    else {
        return Ok(None);
    };
    if let Some(sbx) = crate::sandbox::active() {
        if !sbx.has_transcript(&session_id) {
            progress(&format!(
                "the builder's earlier session {session_id} has no transcript in the sandbox; \
                 starting a fresh one"
            ));
            return Ok(None);
        }
    }
    let prompt = match (escalated, item.answers.last()) {
        (true, Some(answer)) => format!(
            "A person answered Paja's question about this issue.\n{}\nContinue with that.",
            show_answer(answer)
        ),
        _ => "Your earlier session was interrupted. Check `git status` and `git log`, finish the \
              issue, and commit."
            .to_string(),
    };
    progress("resuming the builder's own session from the earlier run");
    Ok(Some(Carried {
        totals: recorded_totals(&events, &item.repo, item.number, &session_id),
        session_id,
        prompt,
    }))
}

/// Record that a step of `round` starts, for anyone following the run: an agent step of
/// `role` (with the model `.paja/models` gives it), or `checks`.
fn record_step(
    home: &Home,
    item: &WorkItem,
    round: u32,
    role: &str,
    models: Option<&Models>,
    reviewer: Option<u32>,
    reviewers: Option<u32>,
) -> Result<()> {
    let model = models.and_then(|m| m.chosen(role)).map(|c| c.model.clone());
    record(
        home,
        EV_STEP,
        json!({
            "repo": item.repo, "number": item.number, "round": round, "role": role,
            "model": model, "reviewer": reviewer, "reviewers": reviewers,
        }),
    )
}

/// Record a reviewer's verdict as soon as it is in, so someone following the run sees which
/// reviews of the round are still out.
fn record_reviewed(
    home: &Home,
    item: &WorkItem,
    round: u32,
    reviewer: u32,
    verdict: &str,
) -> Result<()> {
    record(
        home,
        EV_REVIEWED,
        json!({
            "repo": item.repo, "number": item.number, "round": round,
            "reviewer": reviewer, "verdict": verdict,
        }),
    )
}

/// Record that a builder session is about to start, before it does.
fn record_step_started(
    home: &Home,
    env: &Env,
    item: &WorkItem,
    round: u32,
    role: &str,
    session_id: &str,
    resumed: bool,
) -> Result<()> {
    record(
        home,
        EV_STEP_STARTED,
        json!({
            "repo": item.repo,
            "number": item.number,
            "round": round,
            "role": role,
            "session_id": session_id,
            "branch": env.branch,
            "resumed": resumed,
        }),
    )
}

/// When the session's work was thrown away, say so in the log: that session is never resumed.
fn note_discarded<T>(
    home: &Home,
    item: &WorkItem,
    session_id: &str,
    result: Result<T>,
) -> Result<T> {
    if let Err(err) = &result {
        if err.chain().any(|e| e.is::<HookNeverRan>()) {
            let _ = record(
                home,
                EV_STEP_DISCARDED,
                json!({ "repo": item.repo, "number": item.number, "session_id": session_id }),
            );
        }
    }
    result
}

/// The `.paja/models` role a review runs as. The first reviewer of a round reads the most and
/// keeps `review`. A normal-risk review of only the fix, and the second high-risk reviewer (a
/// second opinion on what the first also reads in full), can run on a cheaper model.
fn review_role(risky: bool, reviewer: usize, fix_only: bool) -> &'static str {
    match (risky, reviewer) {
        (true, 0) => "review",
        (true, _) => "review-2",
        (false, _) if fix_only => "re-review",
        (false, _) => "review",
    }
}

/// The programs as prose: "local git", "local git and `npm`", "local git, `npm` and `node`".
fn programs_prose(programs: &[String]) -> String {
    let names: Vec<String> = programs.iter().map(|p| format!("`{p}`")).collect();
    match names.split_last() {
        None => "local git".to_string(),
        Some((last, [])) => format!("local git and {last}"),
        Some((last, rest)) => format!("local git, {} and {last}", rest.join(", ")),
    }
}

/// The `--allowedTools` entries for the project's programs.
fn program_tools(programs: &[String]) -> Vec<String> {
    programs.iter().map(|p| format!("Bash({p} *)")).collect()
}

/// What the builder is asked. `programs` are the ones `.paja/commands` lets it run.
fn builder_prompt(item: &WorkItem, programs: &[String], checks: &[String]) -> String {
    let checks = checks
        .iter()
        .map(|c| format!("`{c}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are Paja's builder. Implement this GitHub issue in the current checkout, following \
         CLAUDE.md. Keep the change small and focused on the issue. Run the checks CLAUDE.md lists \
         (Vasara's checks, every command in `.paja/checks` on the default branch: {checks}), \
         and commit your work on the current branch (never push, never switch branches). Commit \
         with `git add -A && git commit -m '<message>'`, the message one single-quoted argument \
         that ends with `Closes #{number}`. Read and change files with the file tools; Bash is \
         only for {programs}, and Paja's deny-list hook blocks anything else (no `$(`, \
         heredocs or redirects). When output is long, pipe it through `tail -n 80` or `grep`. If the \
         issue leaves a decision only the person can make (what to build or what is out of scope, \
         not how to build it), don't put the question in code, tests or commits: commit nothing, \
         and end your final message with a line that starts with `{ASK}` followed by the \
         question, the options and your recommendation. Paja then asks the person on the \
         issue.\n\nIssue #{number}: {title}\n\n{body}\n{answers}",
        number = item.number,
        programs = programs_prose(programs),
        title = item.title,
        body = item.body,
        answers = answers(item)
    )
}

/// What one build step gave: its usage this round, its running totals (for a later resume),
/// and the question it asked instead of building, if any.
type Built = (StepUsage, StepUsage, Option<String>);

#[allow(clippy::too_many_arguments)]
fn build(
    home: &Home,
    env: &Env,
    item: &WorkItem,
    base: &str,
    feedback: &str,
    models: &Models,
    role: &str,
    round: u32,
    resume: Option<&StepUsage>,
    carried: Option<&Carried>,
    spent: &mut Vec<StepUsage>,
    programs: &[String],
    checks: &[String],
) -> Result<Built> {
    let mut prompt = builder_prompt(item, programs, checks);
    let again = format!(
        "This is another round on the same branch (based on {base}). Address this first:\n\n{feedback}\n"
    );
    if !feedback.is_empty() {
        prompt.push_str(&format!("\n{again}"));
    }
    let program_tools = program_tools(programs);
    let mut tools: Vec<&str> = vec!["Read", "Edit", "Write", "Glob", "Grep"];
    tools.extend(program_tools.iter().map(String::as_str));
    tools.extend([
        "Bash(git add *)",
        "Bash(git commit *)",
        "Bash(git rm *)",
        "Bash(git status*)",
        "Bash(git diff*)",
        "Bash(git show*)",
        "Bash(git log*)",
        "Bash(tail *)",
        "Bash(head *)",
        "Bash(grep *)",
        "Bash(wc *)",
        "Bash(pwd)",
    ]);
    // A fix round resumes the builder's own session, which keeps what it learned and, on a
    // subscription, reads its earlier turns from the prompt cache. It gets only the feedback.
    // Every other flag, the hook included, is the same as for a fresh session.
    // The first build of a run resumes an earlier run's session the same way (an interruption
    // or an answer), with the short prompt `carried` holds instead of the issue.
    let session = resume.and_then(|earlier| earlier.session_id.as_deref());
    let attempt = match (session, feedback.is_empty(), carried) {
        (Some(id), false, _) => Some((id, again.as_str(), resume)),
        (_, true, Some(c)) => Some((c.session_id.as_str(), c.prompt.as_str(), c.totals.as_ref())),
        _ => None,
    };
    if let Some((id, resume_prompt, basis)) = attempt {
        let mut args = models.args(role);
        args.extend(["--resume".to_string(), id.to_string()]);
        record_step_started(home, env, item, round, role, id, true)?;
        let tried = claude(home, env, base, resume_prompt, &tools, None, false, &args);
        match note_discarded(home, item, id, tried) {
            Ok(out) => {
                let raw = StepUsage::from_result("build", None, models, role, &out);
                let step = match basis {
                    Some(earlier) => raw.since(earlier),
                    None => StepUsage {
                        resumed: true,
                        ..raw.clone()
                    },
                };
                return Ok((step, raw, builder_question(&out)));
            }
            Err(err) => {
                let Some(ClaudeFailed(_, result)) =
                    err.chain().find_map(|e| e.downcast_ref::<ClaudeFailed>())
                else {
                    return Err(err);
                };
                if let Some(out) = result {
                    // The failed attempt may still have spent something; it counts too, whether
                    // or not a fresh session follows.
                    let raw = StepUsage::from_result("build", None, models, role, out);
                    spent.push(match basis {
                        Some(earlier)
                            if earlier.session_id.is_some()
                                && earlier.session_id == raw.session_id =>
                        {
                            raw.since(earlier)
                        }
                        _ => raw,
                    });
                    // A fresh session would fail the same way: the run keeps the work and waits
                    // for the window (see `run_item`).
                    if usage_limit(&err).is_some() {
                        return Err(err);
                    }
                    // A sign-in that no longer works: stop and say what to do.
                    if let Some(fix) = not_retried(out) {
                        return Err(err.context(fix));
                    }
                }
                progress(&format!(
                    "resuming the builder's session failed ({}); starting a fresh one",
                    shown_line(&format!("{err:#}"))
                ));
            }
        }
    } else if !feedback.is_empty() {
        progress("no builder session to resume; starting a fresh one");
    }
    // Mestari names the session itself and records it first, so a run that is killed before
    // the session prints its result can still resume it.
    let id = new_session_id();
    let mut args = vec!["--session-id".to_string(), id.clone()];
    args.extend(models.args(role));
    record_step_started(home, env, item, round, role, &id, false)?;
    let tried = note_discarded(
        home,
        item,
        &id,
        claude(home, env, base, &prompt, &tools, None, false, &args),
    );
    let out = match tried {
        Ok(out) => out,
        Err(err) => {
            // A usage limit still spent something.
            if let Some(UsageLimit {
                result: Some(result),
                ..
            }) = usage_limit(&err)
            {
                spent.push(StepUsage::from_result("build", None, models, role, &result));
            }
            return Err(err);
        }
    };
    let raw = StepUsage::from_result("build", None, models, role, &out);
    Ok((raw.clone(), raw, builder_question(&out)))
}

/// What a reviewer is asked. `checked` is the commit Vasara's checks passed at, which is the
/// commit the reviewer judges.
#[allow(clippy::too_many_arguments)]
fn review_prompt(
    item: &WorkItem,
    base: &str,
    checked: &str,
    prior: Option<&Prior>,
    noted: &[Finding],
    diff: &str,
    checks: &[String],
    programs: &[String],
) -> String {
    // A first review reads the whole change; a later one judges only the fix for the blockers
    // an earlier review named, so review rounds converge instead of starting over.
    let scope = match prior {
        None if noted.is_empty() => format!(
            "The change is the diff above (`git diff {base}...HEAD`). Read it cold against the \
             issue."
        ),
        None => format!(
            "The change is the diff above (`git diff {base}...HEAD`). Read it cold against the \
             issue. An earlier review of it noted these findings outside the change; repeat in \
             your `follow_ups` the ones that still apply:\n\n{}\n",
            noted
                .iter()
                .map(Finding::shown)
                .collect::<Vec<_>>()
                .join("\n\n")
        ),
        Some(prior) => format!(
            "An earlier review of this change (at {reviewed}) asked for these fixes:\n\n{blockers}\n\n\
             Judge only the fix, the diff above (`git diff {reviewed}..HEAD`). Check that each of \
             those is settled and that the fix adds no problem. That review read the whole change \
             and named every finding it had, so don't reopen the rest. It also noted these \
             findings outside the change; repeat in your `follow_ups` the ones that still apply:\n\n{follow_ups}\n",
            reviewed = prior.reviewed,
            blockers = prior.blockers,
            follow_ups = if prior.follow_ups.is_empty() {
                "(none)".to_string()
            } else {
                prior
                    .follow_ups
                    .iter()
                    .map(Finding::shown)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        ),
    };
    // What every reviewer of a round shares comes first (issue, answers, diff), and the
    // role-specific text last, so a second reviewer on the same model reuses the cached prefix.
    format!(
        "Issue #{number}: {title}\n\n{body}\n{answers}\n{diff}\n\n\
         You are an independent reviewer for this project. You did not write this change; decide \
         whether it lands.\n\n{scope} You already have the diff, so read only the surrounding \
         code you need. Judge it by the rules in CLAUDE.md as they are on the default branch: the CLAUDE.md in \
         your system prompt is the default branch's copy, not the branch's, so don't read it again.\n\n\
         Judge the change against its issue. Verdicts: `land` if it does what the issue asks and \
         has no blocker; `changes` only for blockers, naming every blocker in the whole diff in \
         `comments` (a later review reads only the fix); `escalate` with a question only when the issue's \
         intent is unclear or the change would weaken a check (a person must decide). A blocker \
         is a bug in what the issue asks for, a security hole the change opens, a broken \
         CLAUDE.md rule, or missing tests for the new behaviour. Anything else goes in \
         `follow_ups`, one finding each, starting with a short title line, at most five, each \
         marked with its `kind`: `bug` for a wrong result in the change, `risk` for a security \
         or data-loss risk or a missing test for behaviour that could break, `nit` for style, \
         naming, comment wording or a small refactor, `outside` for a real bug in code the \
         change doesn't touch. Paja sends every finding except `outside` back to the builder, \
         who fixes it in this change (so a `land` verdict with such findings runs another \
         round), and no issue is filed for any. Leave out ideas that are none of these, and a \
         nit that isn't worth a round. `outside` findings and any still open at the round \
         limit are listed in the public pull request, so never copy secrets, environment \
         details or run output into them.\n\n\
         Vasara's checks (every command in `.paja/checks` on the default branch: {checks}) already passed at \
         {checked}, the commit you are judging, so don't run them again. {probe}Read-only git \
         is fine; you can't change files.\n",
        probe = if programs.is_empty() {
            "Bash is limited to local git, so read the code to judge a behaviour. ".to_string()
        } else {
            format!(
                "To probe a behaviour, run one named test or the project itself, with Bash \
                 limited to {} and long output piped through `tail -n 80` or `grep`. ",
                programs_prose(programs)
            )
        },
        checks = checks
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", "),
        number = item.number,
        title = item.title,
        body = item.body,
        answers = answers(item)
    )
}

/// The most characters of diff a review prompt carries; the rest is left to the reviewer.
const REVIEW_DIFF_CAP: usize = 60_000;

/// `git diff --stat` and the diff for `range`, as the review prompt shows them. Above `cap`
/// characters, whole files are included in order until the next would pass it, and the rest
/// are named for the reviewer to read itself.
fn review_diff(dir: &Path, range: &str, cap: usize) -> Result<String> {
    // Renames pinned on, so the names and the sections below agree whatever the machine's git
    // configuration says.
    let flags = [
        "-c",
        "diff.renames=true",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
    ];
    let run = |extra: &[&str]| {
        let mut args = flags.to_vec();
        args.extend_from_slice(extra);
        args.push(range);
        git(dir, &args)
    };
    let stat = run(&["--stat"])?;
    let full = run(&[])?;
    let mut text = format!("Diff stat for `{range}`:\n\n{stat}\n\n");
    if full.chars().count() <= cap {
        text.push_str(&format!("The diff:\n\n{full}"));
        return Ok(text);
    }
    // The names come from git itself (`-z`: no quoting, any character in a path), in the order
    // the diff lists its files.
    let names = run(&["--name-only", "-z"])?;
    let names: Vec<&str> = names.split('\0').filter(|n| !n.is_empty()).collect();
    let mut shown = String::new();
    let mut used = 0usize;
    let mut skipped = Vec::new();
    let mut cut = None;
    let mut files: Vec<String> = Vec::new();
    for line in full.lines() {
        if line.starts_with("diff --git ") || files.is_empty() {
            files.push(String::new());
        }
        let last = files.last_mut().expect("a file was just pushed");
        last.push_str(line);
        last.push('\n');
    }
    let labels = section_names(&names, &files);
    let mut full_so_far = false;
    for (index, file) in files.iter().enumerate() {
        let name = labels[index];
        let size = file.chars().count();
        if !full_so_far && used + size <= cap {
            shown.push_str(file);
            used += size;
        } else {
            // Once one file doesn't fit, the later ones are left out too, so the diff shown
            // is a prefix of the whole.
            if !full_so_far && shown.is_empty() {
                // The first file alone is over the cap: show its head, cut at the cap.
                shown = file.chars().take(cap).collect();
                cut = Some(name);
            }
            full_so_far = true;
            skipped.push(format!("{name} ({} lines)", file.lines().count()));
        }
    }
    let note = match cut {
        Some(name) => format!(
            "The first file, {name}, is over the cap by itself, so only its first {cap} \
             characters are here. "
        ),
        None => String::new(),
    };
    text.push_str(&format!(
        "The diff is over {cap} characters, so only its first files are here. {note}\n\n{shown}\n\
         Not shown (or cut), because of the size cap; you must read these files yourself (`git \
         diff {range} -- <file>`): {}\n",
        skipped.join(", ")
    ));
    Ok(text)
}

/// The name of each of the diff's `files` (one section per file): `names`, from `git diff
/// --name-only -z`, matched by position. Should git ever list a different number of them, each
/// section is named by its own `diff --git` line instead, which may be quoted or ambiguous for
/// odd paths but never names another file.
fn section_names<'a>(names: &[&'a str], files: &'a [String]) -> Vec<&'a str> {
    if names.len() == files.len() {
        return names.to_vec();
    }
    files
        .iter()
        .map(|file| {
            file.lines()
                .next()
                .and_then(|l| l.strip_prefix("diff --git "))
                .unwrap_or("(unknown file)")
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn review(
    home: &Home,
    env: &Env,
    item: &WorkItem,
    base: &str,
    checked: &str,
    prior: Option<&Prior>,
    noted: &[Finding],
    reviewer: u32,
    models: &Models,
    role: &str,
    focused: bool,
    checks: &[String],
    programs: &[String],
) -> Result<(Review, StepUsage)> {
    // The reviewer judges the commit the checks passed at, and nothing else.
    let head = git(&env.path, &["rev-parse", "HEAD"])?;
    if head != checked {
        bail!(
            "the worktree {} is at {head}, not at {checked} where Vasara's checks passed, so it \
             was not reviewed; run again",
            env.path.display()
        );
    }
    let before = checked.to_string();
    let range = match prior {
        None => format!("{base}...HEAD"),
        Some(prior) => format!("{}..HEAD", prior.reviewed),
    };
    let diff = review_diff(&env.path, &range, REVIEW_DIFF_CAP)?;
    let mut prompt = review_prompt(item, base, &before, prior, noted, &diff, checks, programs);
    if focused {
        prompt.push_str(&format!(
            "\nAn earlier review of this commit stopped at its turn limit of {} before giving a \
             verdict. Stay focused: read the diff and only the code it touches, probe at most a \
             few behaviours, and give your verdict well before the limit.\n",
            models.max_turns(role)
        ));
    }
    let schema = json!({
        "type": "object",
        "properties": {
            "verdict": {"type": "string", "enum": ["land", "changes", "escalate"]},
            "summary": {"type": "string"},
            "comments": {"type": "string"},
            "question": {"type": "string"},
            "follow_ups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["bug", "risk", "nit", "outside"]},
                        "finding": {"type": "string"}
                    },
                    "required": ["kind", "finding"]
                }
            }
        },
        "required": ["verdict", "summary"]
    });
    let program_tools = program_tools(programs);
    let mut tools: Vec<&str> = vec!["Read", "Glob", "Grep"];
    tools.extend(program_tools.iter().map(String::as_str));
    tools.extend([
        "Bash(git status*)",
        "Bash(git diff*)",
        "Bash(git log*)",
        "Bash(tail *)",
        "Bash(head *)",
        "Bash(grep *)",
        "Bash(wc *)",
        "Bash(pwd)",
        "Bash(git show*)",
    ]);
    let out = claude(
        home,
        env,
        base,
        &prompt,
        &tools,
        Some(&schema),
        true,
        &models.args(role),
    );
    let out = match out {
        // No git at all now, not even to reset: a planted filter would run.
        Err(err) if is_tampered(&err) => return Err(err),
        out => out,
    };
    // A reviewer changes nothing, but the code its builds and tests run can (a lockfile, even
    // a commit). Put the branch back where the review started, so none of it lands unread.
    // Ignored files such as `target/` stay.
    let reset = put_back(env, &before);
    let out = match (out, reset) {
        (Err(err), Err(reset)) => {
            return Err(err.context(format!("and resetting the worktree failed too: {reset:#}")))
        }
        (out, reset) => {
            reset?;
            out?
        }
    };
    let structured = out
        .get("structured_output")
        .cloned()
        .context("the reviewer returned no verdict")?;
    let mut review: Review =
        serde_json::from_value(structured).context("the reviewer's verdict has the wrong shape")?;
    // A verdict without its details still has to give the builder or the person something.
    if review.comments.trim().is_empty() {
        review.comments = review.summary.clone();
    }
    if review.question.trim().is_empty() {
        review.question = review.summary.clone();
    }
    let used = StepUsage::from_result("review", Some(reviewer), models, role, &out);
    Ok((review, used))
}

/// Commit anything the builder left uncommitted, so the checks judge what will land.
fn commit_leftovers(env: &Env, item: &WorkItem) -> Result<()> {
    if git(&env.path, &["status", "--porcelain"])?.is_empty() {
        return Ok(());
    }
    git(&env.path, &["add", "-A"])?;
    git(
        &env.path,
        &[
            "commit",
            "-q",
            "-m",
            &format!("{}\n\nCloses #{}", item.title, item.number),
        ],
    )?;
    Ok(())
}

/// Whether a changed path is high risk: it starts with one of `prefixes`, or, for a prefix
/// that is a single name (`.cargo/`, `CLAUDE.md`), any directory or file in it is that name,
/// or that name with an extension (`rust-toolchain.toml`), in any letter case, since nested
/// configuration counts as much as the top level.
fn is_high_risk(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        let name = prefix.trim_end_matches('/').to_lowercase();
        path.to_lowercase().starts_with(&prefix.to_lowercase())
            || !name.contains('/')
                && path
                    .split('/')
                    .map(str::to_lowercase)
                    .any(|part| part == name || part.starts_with(&format!("{name}.")))
    })
}

/// Run Vasara's checks on `checked` in the environment, under a `GitGuard`. The failure to send
/// the builder, or None when they pass. The checks run the branch's code, which could change the
/// tree or even commit: that is put back and fails them, so nothing lands that they didn't judge.
fn vasara(
    home: &Home,
    env: &Env,
    item: &WorkItem,
    round: u32,
    checks: &[String],
    checked: &str,
) -> Result<Option<String>> {
    // Files marked assume-unchanged or skip-worktree hide from `git status`. Sparse checkout
    // marks every file outside its cone skip-worktree, so the checks are judged by whether they
    // changed the flags. Any flag the builder's code set goes first: a fresh index keeps only
    // the ones sparse checkout gives.
    let mut flags = git(&env.path, &["ls-files", "-v"])?;
    if has_index_flags(&flags) {
        put_back(env, checked)?;
        flags = git(&env.path, &["ls-files", "-v"])?;
    }
    stop_point()?;
    let guard = GitGuard::new(&env.path)?;
    progress(&format!(
        "running Vasara's checks ({} command{})",
        checks.len(),
        if checks.len() == 1 { "" } else { "s" }
    ));
    record_step(home, item, round, "checks", None, None, None)?;
    let result = run_checks(&env.path, checks);
    guard.check("Vasara's checks")?;
    let mut failed = result;
    let moved = git(&env.path, &["rev-parse", "HEAD"])? != checked
        || git(&env.path, &["ls-files", "-v"])? != flags
        || !git(&env.path, &["status", "--porcelain"])?.is_empty();
    if moved {
        put_back(env, checked)?;
        if let Ok(failure) = &mut failed {
            failure.get_or_insert_with(|| {
                "Running the checks changed the worktree or its commits; the change was put back. \
                 Tests and builds must not write to the source tree or commit."
                    .into()
            });
        }
    }
    failed
}

/// Run each check in the environment. Returns the first failure's command and output. The
/// checks run the branch's code, so they get no more credentials than the agent that wrote it.
fn run_checks(dir: &Path, checks: &[String]) -> Result<Option<String>> {
    // Read before any check runs: later, the branch's code may have changed the configuration.
    let identity = git_identity(dir);
    // The sandbox has its own `PAJA_HOME` (`sandbox::SCRATCH`), so no host directory is made then.
    let sbx = crate::sandbox::active();
    let scratch = if sbx.is_none() {
        tempfile_dir(dir)?
    } else {
        PathBuf::new()
    };
    let result = (|| {
        for check in checks {
            let mut cmd = match &sbx {
                Some(sbx) => in_sandbox(sbx, dir, identity.clone(), Ok(check))?,
                None => {
                    let mut cmd = crate::platform::shell(check)?;
                    cmd.current_dir(dir);
                    without_credentials(&mut cmd, &scratch, identity.clone());
                    cmd
                }
            };
            // Without the sandbox the checks keep the owner's HOME, as agents do: toolchain
            // managers find cargo through it, and credential files there stay reachable.
            let output =
                crate::platform::output_until_stopped(cmd.stdin(std::process::Stdio::null()))
                    .with_context(|| format!("running check `{check}`"))?;
            if !output.status.success() {
                return Ok(Some(format!("`{check}` failed:\n\n{}", tail(&output))));
            }
        }
        Ok(None)
    })();
    if sbx.is_none() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    result
}

fn record(home: &Home, kind: &str, data: Value) -> Result<()> {
    let lock = home.lock()?;
    home.events().append(&lock, kind, data)?;
    Ok(())
}

/// What the round after this one starts from, recorded in `run.round` so a run that stops can
/// be resumed there.
struct Next<'a> {
    /// What the builder is asked to address.
    feedback: &'a str,
    /// What the reviewers asked for, which the next review narrows itself to.
    prior: Option<&'a Prior>,
    /// The findings outside the change the latest review noted, kept for the next review to
    /// keep or drop; recorded only with no `prior`, whose `follow_ups` hold them otherwise.
    noted: &'a [Finding],
}

/// A round that ended without a next round: a question, a landing, an error or a stop.
fn record_round(
    home: &Home,
    item: &WorkItem,
    round: u32,
    verdict: &str,
    detail: &str,
    steps: &mut Vec<StepUsage>,
) -> Result<()> {
    record_round_next(home, item, round, verdict, detail, steps, None)
}

/// A round that ended without a next round, with the findings outside the change its review
/// noted (`noted`, an empty list when there are none), for a resumed run of that round. A round
/// every reviewer approved also records `approved` (an [`Approval`]), so a resumed run lands it
/// without another review.
#[allow(clippy::too_many_arguments)]
fn record_round_noted(
    home: &Home,
    item: &WorkItem,
    round: u32,
    verdict: &str,
    detail: &str,
    steps: &mut Vec<StepUsage>,
    noted: &[Finding],
    approved: Option<Approval>,
) -> Result<()> {
    let steps = std::mem::take(steps);
    let mut data = json!({
        "repo": item.repo,
        "number": item.number,
        "round": round,
        "verdict": verdict,
        "detail": clip(detail),
        "steps": steps,
    });
    data["noted"] = json!(noted);
    // Every reviewer approved: the commit, how many approved it, and the findings the change
    // lands with, in it and outside it.
    if let Some(approval) = approved {
        data["approved"] = json!(approval);
    }
    record(home, EV_ROUND, data)
}

/// A round after which the loop goes on: `next` is what the following round needs.
fn record_round_next(
    home: &Home,
    item: &WorkItem,
    round: u32,
    verdict: &str,
    detail: &str,
    steps: &mut Vec<StepUsage>,
    next: Option<Next>,
) -> Result<()> {
    // Recorded once: an error later in the run must not count these steps again.
    let steps = std::mem::take(steps);
    let mut data = json!({
        "repo": item.repo,
        "number": item.number,
        "round": round,
        "verdict": verdict,
        "detail": clip(detail),
        "steps": steps,
    });
    if let Some(next) = next {
        data["next"] = json!({
            "feedback": clip(next.feedback),
            "prior": next.prior.map(|p| json!({
                "reviewed": p.reviewed,
                "blockers": clip(&p.blockers),
                "follow_ups": p.follow_ups,
            })),
        });
        // With a prior, its `follow_ups` hold them.
        if next.prior.is_none() && !next.noted.is_empty() {
            data["next"]["noted"] = json!(next.noted);
        }
    }
    record(home, EV_ROUND, data)
}

/// Vasara's checks passed on `commit`, with the default branch at `base`: recorded before the
/// review, so a run that stops in it can go straight to the review again. `landing`: the commit
/// is Paja's own merge of the default branch into an approved change, checked again before it
/// lands, so the approval holds for it.
fn record_checked(
    home: &Home,
    item: &WorkItem,
    round: u32,
    commit: &str,
    base: &str,
    checks_passed: usize,
    landing: bool,
) -> Result<()> {
    record(
        home,
        EV_CHECKED,
        json!({
            "repo": item.repo, "number": item.number, "round": round,
            "commit": commit, "base": base, "checks_passed": checks_passed,
            "landing": landing,
        }),
    )
}

/// A round's review that every reviewer approved, as `run.round` records it in `approved`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Approval {
    /// The commit the review read.
    reviewed: String,
    /// The commit the change lands as: the one reviewed, or Paja's own merge of the default
    /// branch into it at the turn to land, checked again.
    commit: String,
    /// How many reviewers approved it: a change that needs more now is reviewed again.
    reviewers: usize,
    /// The findings it lands with, in the change and outside it.
    findings: Vec<Finding>,
}

/// Where a run that stopped left an item, rebuilt from the item's events.
struct ResumePoint {
    /// The round to go on in: the one that was cut off.
    round: u32,
    /// What the builder is asked to address in it.
    feedback: String,
    prior: Option<Prior>,
    /// With no `prior`, the findings outside the change an earlier review noted.
    noted: Vec<Finding>,
    /// The commit the checks passed on in that round, and the default branch's tip they ran
    /// against.
    checked: Option<(String, String)>,
    /// The round's review approved the change, as [`Approval::commit`] stands.
    approved: Option<Approval>,
    /// Where the run left off, in words.
    stopped_in: String,
}

/// The resume point of an item whose run stopped: every run event of the item since it was
/// last queued, claimed or answered counts, not only the last run's, so an item that is
/// interrupted again before it records a round keeps its round count and its blockers.
fn resume_point(events: &[crate::events::Event], repo: &str, number: u64) -> ResumePoint {
    let mut point = ResumePoint {
        round: 1,
        feedback: String::new(),
        prior: None,
        noted: Vec::new(),
        checked: None,
        approved: None,
        stopped_in: String::new(),
    };
    for event in jono::item_events(events, repo, number) {
        let data = &event.data;
        match event.kind.as_str() {
            jono::EV_STATUS | jono::EV_ANSWERED | jono::EV_REOPENED | EV_LANDED | EV_ESCALATED => {
                point = ResumePoint {
                    round: 1,
                    feedback: String::new(),
                    prior: None,
                    noted: Vec::new(),
                    checked: None,
                    approved: None,
                    stopped_in: String::new(),
                };
            }
            EV_ROUND => {
                let round = data["round"].as_u64().unwrap_or(0) as u32;
                match data.get("next").filter(|n| n.is_object()) {
                    Some(next) => {
                        point.round = round + 1;
                        point.feedback = next["feedback"].as_str().unwrap_or_default().to_string();
                        point.prior = serde_json::from_value(next["prior"].clone()).ok();
                        point.noted =
                            serde_json::from_value(next["noted"].clone()).unwrap_or_default();
                        point.checked = None;
                        point.approved = None;
                        point.stopped_in = format!(
                            "round {round} ended with {}",
                            data["verdict"].as_str().unwrap_or("no verdict")
                        );
                    }
                    None => {
                        point.round = point.round.max(round);
                        // A review that ended the round (it records `noted`) says whether
                        // every reviewer approved: a later one that didn't (a split, an
                        // escalation) takes back an earlier approval. A stop or an error records
                        // no `noted` and leaves it; a new round, a fresh start or checks on
                        // another commit take it back too.
                        if data["noted"].is_array() {
                            point.approved = serde_json::from_value(data["approved"].clone()).ok();
                        }
                        // A round that ended approved names the latest outside findings, newer
                        // than what the round before it handed on, even when there are none. A
                        // round recorded without the field (a stop, an error, an older log)
                        // leaves the earlier ones.
                        if let Ok(noted) =
                            serde_json::from_value::<Vec<Finding>>(data["noted"].clone())
                        {
                            if let Some(prior) = &mut point.prior {
                                prior.follow_ups = noted.clone();
                            }
                            point.noted = noted;
                        }
                    }
                }
            }
            EV_CHECKED if data["round"].as_u64() == Some(point.round as u64) => {
                if let (Some(commit), Some(base)) = (data["commit"].as_str(), data["base"].as_str())
                {
                    point.checked = Some((commit.to_string(), base.to_string()));
                    // Checks on Paja's own merge of the approved change (`landing`) move the
                    // approval to it; checks on any other commit mean the approved one was
                    // built on, so it no longer holds.
                    if let Some(approval) = &mut point.approved {
                        if data["landing"] == true {
                            approval.commit = commit.to_string();
                        } else if approval.commit != commit {
                            point.approved = None;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    point
}

/// The first characters of a commit id, to name it in a progress line.
fn short(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Whether the worktree is exactly `commit`: that commit checked out, nothing changed or
/// untracked, no merge under way and no index flag hiding a file.
fn untouched_at(env: &Env, commit: &str) -> Result<bool> {
    Ok(git(&env.path, &["rev-parse", "HEAD"])? == commit
        && git(&env.path, &["status", "--porcelain"])?.is_empty()
        && !merging(&env.path)
        && !has_index_flags(&git(&env.path, &["ls-files", "-v"])?))
}

fn summaries(reviews: &[Review]) -> String {
    reviews
        .iter()
        .enumerate()
        .map(|(i, r)| format!("Reviewer {}: {} — {}", i + 1, r.verdict, r.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read a file as it is on the base commit, or None if it isn't there (see [`ahjo::show_at`]).
fn read_from_base(repo: &Path, base: &str, path: &str) -> Result<Option<String>> {
    ahjo::show_at(repo, base, path)
}

/// Non-empty lines that are not `#` comments.
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn clip(text: &str) -> String {
    const MAX: usize = 4000;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut cut = text.len() - MAX;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("…{}", &text[cut..])
}

fn tail(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    clip(text.trim())
}

/// What bringing a branch up to date with the default branch found.
enum CatchUp {
    /// The branch already holds the default branch's tip, given here.
    Current(String),
    /// The default branch merged in cleanly; its tip, the new base.
    Merged(String),
    /// The merge conflicts in these files. It is left in progress for the builder to finish.
    Conflict(Vec<String>),
}

/// Merge the latest `branch` into the environment's branch, so Vasara's checks, the
/// reviewers and CI judge the change as it would land, and a stale branch never reaches the
/// pull request.
fn catch_up(repo: &Path, env: &Env, branch: &str) -> Result<CatchUp> {
    // A merge an earlier round left unfinished starts over.
    if merging(&env.path) {
        git(&env.path, &["merge", "--abort"])?;
    }
    let tip = fetch_tip(repo, branch).with_context(|| {
        format!("fetching origin/{branch}; check the network and that `origin` is the GitHub repository")
    })?;
    if git(&env.path, &["merge-base", "--is-ancestor", &tip, "HEAD"]).is_ok() {
        return Ok(CatchUp::Current(tip));
    }
    // The person's identity when git has one, so the merge commit is theirs like the build.
    let mut args: Vec<&str> = Vec::new();
    if git(&env.path, &["config", "user.email"]).is_err() {
        args.extend([
            "-c",
            "user.name=Paja",
            "-c",
            "user.email=paja@example.invalid",
        ]);
    }
    let message = format!("Merge {branch} into {}", env.branch);
    args.extend(["merge", "-q", "--no-edit", "-m", &message, &tip]);
    match git(&env.path, &args) {
        Ok(_) => Ok(CatchUp::Merged(tip)),
        Err(err) => {
            let files = unmerged(&env.path)?;
            if files.is_empty() {
                if merging(&env.path) {
                    let _ = git(&env.path, &["merge", "--abort"]);
                }
                return Err(err.context(format!("merging origin/{branch} into {}", env.branch)));
            }
            Ok(CatchUp::Conflict(files))
        }
    }
}

/// What the builder is told when merging the latest `branch` conflicts in `files`, with the
/// review's blockers when they still need fixing. `after_review` is for a conflict at the turn to
/// land, when `branch` moved on after the review rather than since the branch was made.
fn conflict_feedback(
    branch: &str,
    after_review: bool,
    files: &[String],
    prior: Option<&Prior>,
) -> String {
    let when = if after_review {
        "after the review"
    } else {
        "since this branch was made"
    };
    let mut feedback = format!(
        "{branch} moved on {when}, and merging it conflicts in: {}. Paja \
         started the merge: the conflicted files hold conflict markers. Resolve each one, keeping \
         what both sides meant, then `git add` the files and `git commit -m '<message>'` to \
         finish the merge.",
        files.join(", ")
    );
    if let Some(prior) = prior {
        feedback.push_str(&format!(
            "\n\nThe review's blockers still need fixing too:\n\n{}",
            prior.blockers
        ));
    }
    feedback
}

/// What the builder is told about a merge of `branch` still in progress in the environment with
/// files that conflict, or None when there is no such merge.
fn unfinished_merge(env: &Env, branch: &str) -> Result<Option<String>> {
    if !merging(&env.path) {
        return Ok(None);
    }
    let left = unmerged(&env.path)?;
    Ok((!left.is_empty()).then(|| {
        format!(
            "The merge of {branch} isn't finished: these files still have conflicts: {}. Resolve \
             each one, keeping what both sides meant, then `git add` the files and \
             `git commit -m '<message>'` to finish the merge.",
            left.join(", ")
        )
    }))
}

/// Whether a merge is in progress in `dir`.
fn merging(dir: &Path) -> bool {
    git(dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_ok()
}

/// The files with unresolved merge conflicts in `dir`.
fn unmerged(dir: &Path) -> Result<Vec<String>> {
    Ok(lines(&git(
        dir,
        &["diff", "--name-only", "--diff-filter=U"],
    )?))
}

fn run_gh(repo: &Path, args: &[&str]) -> Result<String> {
    let gh = jono::gh_program();
    let output =
        crate::platform::own_process_group(Command::new(&gh).env("GH_PROMPT_DISABLED", "1"))
            .current_dir(repo)
            .args(args)
            .output()
            .with_context(|| {
                format!("running `{gh}` (install the GitHub CLI and run `gh auth login`)")
            })?;
    if !output.status.success() {
        bail!("`{gh} {}` failed: {}", args.join(" "), tail(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Git as Mestari runs it, with Mestari's own credentials. Code run in a worktree could plant
/// hooks or a file-system monitor for it to run, so those are always off; [`GitGuard`] catches
/// the rest of a planted configuration.
fn git_command() -> Command {
    ahjo::git_command()
}

/// Fetch `branch` from origin and return its tip. The tip goes through a ref of this call's
/// own, never `FETCH_HEAD` or `origin/<branch>`, which every fetch in the checkout writes: runs
/// of a pool's lanes fetch in one checkout at once, and one fetch must not read another's
/// result or fail on its ref lock.
pub(super) fn fetch_tip(repo: &Path, branch: &str) -> Result<String> {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // One fetch at a time in this checkout, through to `origin/<branch>`, so the last fetch to
    // move it is the latest one, whatever order the lanes' fetches would have ended in. (A
    // remote that served an older tip than it had, as a lagging replica can, is followed back
    // until the next fetch.)
    let held = fetch_lock(repo);
    let now = crate::events::now_ms();
    let name = format!("refs/paja/fetch/{}-{now}-{n}", std::process::id());
    sweep_fetch_refs(repo, now);
    let fetched = git(
        repo,
        &[
            "fetch",
            "-q",
            "--refmap=",
            "origin",
            &format!("+refs/heads/{branch}:{name}"),
        ],
    )
    .and_then(|_| {
        git(
            repo,
            &["rev-parse", "--verify", &format!("{name}^{{commit}}")],
        )
    });
    let _ = git(repo, &["update-ref", "-d", &name]);
    // The fetch also writes FETCH_HEAD, which nothing here reads (`--no-write-fetch-head`, from
    // git 2.29, would say so, but older gits refuse it).
    //
    // `origin/<branch>` still follows, as a plain fetch would move it: Jono reads `.paja/labels`
    // and the queue file from it outside runs. Under the lock it goes to the tip just fetched,
    // forward, back after a force-push rewound the branch, or onto rewritten history alike.
    // Without the lock (its file couldn't be opened) a slower fetch can't be told from a
    // rewind, so it then never goes back onto a commit it already holds. Either way it moves
    // only from the value just read: a `git fetch` of the person's own at the same moment may
    // win that compare-and-swap, which is left so on purpose, since the next fetch moves it on.
    if let Ok(tip) = &fetched {
        let tracking = format!("refs/remotes/origin/{branch}");
        match git(repo, &["rev-parse", "--verify", "-q", &tracking]) {
            Ok(old) if old == *tip => {}
            Ok(old) => {
                if held.is_some() || git(repo, &["merge-base", "--is-ancestor", tip, &old]).is_err()
                {
                    let _ = git(repo, &["update-ref", &tracking, tip, &old]);
                }
            }
            Err(_) => {
                let _ = git(repo, &["update-ref", &tracking, tip, ""]);
            }
        }
    }
    drop(held);
    fetched
}

/// How long a [`fetch_tip`] waits for another one in the checkout before it goes ahead
/// without the lock, so one fetch stalled on the network doesn't hold up every lane.
const FETCH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The lock that makes [`fetch_tip`]s in the checkout `repo` take turns: `paja-fetch.lock` in
/// its git directory, held until the returned file is dropped. None when the file can't be
/// opened, or another fetch held it for [`FETCH_LOCK_WAIT`] (debug builds:
/// `PAJA_TEST_FETCH_WAIT_MS`), and the fetch then goes ahead on its own.
fn fetch_lock(repo: &Path) -> Option<std::fs::File> {
    let common = git(repo, &["rev-parse", "--git-common-dir"]).ok()?;
    let path = repo.join(common).join("paja-fetch.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    let wait = std::env::var("PAJA_TEST_FETCH_WAIT_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .and_then(|ms| ms.parse().ok())
        .map_or(FETCH_LOCK_WAIT, std::time::Duration::from_millis);
    let started = std::time::Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < wait => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                progress(&format!(
                    "another fetch held {} for {wait:?}; fetching without it",
                    path.display()
                ));
                return None;
            }
            Err(std::fs::TryLockError::Error(err)) => {
                progress(&format!(
                    "couldn't lock {}: {err}; fetching without it",
                    path.display()
                ));
                return None;
            }
        }
    }
}

/// Delete the `refs/paja/fetch/*` refs a run that was killed between its fetch and deleting
/// the ref left behind: any made over an hour ago by the wall clock, which a fetch still in
/// progress is assumed not to be. The sweep runs under the fetch lock when it has it, so no
/// other locked fetch is in progress then: only a fetch that went ahead without the lock (or
/// one from an older Paja), over an hour old by a clock set forward, could lose its ref, and
/// that fetch then fails.
fn sweep_fetch_refs(repo: &Path, now: u64) {
    const HOUR_MS: u64 = 60 * 60 * 1000;
    let Ok(refs) = git(
        repo,
        &["for-each-ref", "--format=%(refname)", "refs/paja/fetch/"],
    ) else {
        return;
    };
    for name in refs.lines() {
        let made = name
            .rsplit('/')
            .next()
            .and_then(|leaf| leaf.split('-').nth(1))
            .and_then(|ms| ms.parse::<u64>().ok());
        if made.is_some_and(|ms| ms.saturating_add(HOUR_MS) < now) {
            let _ = git(repo, &["update-ref", "-d", name]);
        }
    }
}

/// Whether a long-running loop must stop on `err`: today only a tampered repository.
pub fn is_fatal(err: &anyhow::Error) -> bool {
    is_tampered(err)
}

/// Whether `err` is, or was caused by, a [`Tampered`] repository.
pub fn is_tampered(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<Tampered>())
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    pub(super) fn repo_with_commit(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&path)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap()
                .status;
            assert!(status.success(), "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(path.join("f.txt"), "f\n").unwrap();
        git(&["add", "f.txt"]);
        git(&["commit", "-qm", "c"]);
        path
    }

    pub(super) fn work_item(repo: &str, number: u64) -> crate::jono::WorkItem {
        crate::jono::WorkItem {
            repo: repo.into(),
            number,
            title: "t".into(),
            body: String::new(),
            url: String::new(),
            open: true,
            status: crate::jono::Status::Active,
            note: None,
            usage: None,
            answers: Vec::new(),
            waiting_on: Vec::new(),
            priority: crate::jono::Priority::Work,
            claim: None,
        }
    }

    #[test]
    fn an_environment_of_another_repository_is_refused_and_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let (a, b) = (
            repo_with_commit(tmp.path(), "a"),
            repo_with_commit(tmp.path(), "b"),
        );
        let item = work_item("o/a", 5);
        let name = item.env_name();
        // As if another repository's item had the same environment name.
        let theirs = crate::ahjo::create(
            &home,
            crate::ahjo::CreateOptions {
                name: &name,
                repo: &b,
                base: None,
            },
        )
        .unwrap();
        let err = super::environment(&home, &a, "HEAD", &item)
            .unwrap_err()
            .to_string();
        assert!(err.contains("belongs to the repository"), "{err}");
        assert!(err.contains(&format!("paja env remove {name}")), "{err}");
        assert!(theirs.path.exists());
        assert_eq!(crate::ahjo::get(&home, &name).unwrap().repo, theirs.repo);
        // The repository it belongs to resumes it.
        let (env, resumed) = super::environment(&home, &b, "HEAD", &item).unwrap();
        assert!(resumed);
        assert_eq!(env.path, theirs.path);
    }

    #[test]
    fn an_item_started_before_the_upgrade_resumes_its_work_n_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let (a, b) = (
            repo_with_commit(tmp.path(), "a"),
            repo_with_commit(tmp.path(), "b"),
        );
        let old = crate::ahjo::create(
            &home,
            crate::ahjo::CreateOptions {
                name: "work-7",
                repo: &a,
                base: None,
            },
        )
        .unwrap();
        let (env, resumed) = super::environment(&home, &a, "HEAD", &work_item("o/a", 7)).unwrap();
        assert!(resumed);
        assert_eq!(env.name, "work-7");
        assert_eq!(env.path, old.path);
        // Another checkout's item #7 doesn't take it over.
        let (other, resumed) = super::environment(&home, &b, "HEAD", &work_item("o/b", 7)).unwrap();
        assert!(!resumed);
        assert_ne!(other.name, "work-7");
    }

    #[test]
    fn an_item_whose_work_n_environment_was_removed_resumes_its_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let a = repo_with_commit(tmp.path(), "a");
        let old = crate::ahjo::create(
            &home,
            crate::ahjo::CreateOptions {
                name: "work-7",
                repo: &a,
                base: None,
            },
        )
        .unwrap();
        std::fs::write(old.path.join("draft.txt"), "the builder's work\n").unwrap();
        for args in [
            &["add", "draft.txt"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                "draft",
            ],
        ] {
            git(&old.path, args).unwrap();
        }
        crate::ahjo::remove(&home, "work-7", false).unwrap();
        let item = work_item("o/a", 7);
        // Without a run of the item recorded in `work-7`, the branch isn't known to be its.
        let (env, resumed) = super::environment(&home, &a, "HEAD", &item).unwrap();
        assert!(!resumed);
        crate::ahjo::remove(&home, &env.name, true).unwrap();
        git(&a, &["branch", "-q", "-D", &env.branch]).unwrap();
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                EV_STARTED,
                json!({ "repo": "o/a", "number": 7, "env": "work-7", "base": "HEAD" }),
            )
            .unwrap();
        drop(lock);
        let (env, resumed) = super::environment(&home, &a, "HEAD", &item).unwrap();
        assert!(resumed);
        assert_eq!(env.name, item.env_name());
        assert!(env.path.join("draft.txt").exists());
        // Carried over once: the old branch is now the item's own.
        assert!(git(
            &a,
            &["rev-parse", "--verify", "--quiet", "refs/heads/paja/work-7"]
        )
        .is_err());
        // So once the own branch goes (a landing deletes it), the next run starts fresh.
        crate::ahjo::remove(&home, &env.name, true).unwrap();
        git(&a, &["branch", "-q", "-D", &env.branch]).unwrap();
        let (env, resumed) = super::environment(&home, &a, "HEAD", &item).unwrap();
        assert!(!resumed);
        assert!(!env.path.join("draft.txt").exists());
    }

    #[test]
    fn an_old_work_n_branch_is_carried_over_only_when_the_log_shows_it_is_the_items() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let a = repo_with_commit(tmp.path(), "a");
        let item = work_item("o/a", 7);
        let fresh = |home: &crate::home::Home| {
            let (env, resumed) = super::environment(home, &a, "HEAD", &item).unwrap();
            assert!(!resumed);
            crate::ahjo::remove(home, &env.name, true).unwrap();
            git(&a, &["branch", "-q", "-D", &env.branch]).unwrap();
            git(
                &a,
                &["rev-parse", "--verify", "--quiet", "refs/heads/paja/work-7"],
            )
            .unwrap();
        };
        let append = |kind: &str, data: Value| {
            let lock = home.lock().unwrap();
            home.events().append(&lock, kind, data).unwrap();
        };
        let started =
            |repo: &str| json!({ "repo": repo, "number": 7, "env": "work-7", "base": "x" });
        append(EV_STARTED, started("o/a"));
        // Checked out in a worktree no environment records (its repository moved, say).
        let stray = tmp.path().join("stray");
        git(
            &a,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "paja/work-7",
                stray.to_str().unwrap(),
            ],
        )
        .unwrap();
        fresh(&home);
        git(&a, &["worktree", "remove", stray.to_str().unwrap()]).unwrap();
        // Still held by a `work-7` environment from another checkout.
        let elsewhere = tmp.path().join("elsewhere");
        append(
            crate::ahjo::EV_CREATED,
            json!({
                "name": "work-7", "repo": elsewhere, "path": elsewhere.join("w"),
                "branch": "paja/work-7", "base_commit": "abc", "port_base": 20_000,
                "port_count": 100, "created_ms": 1
            }),
        );
        fresh(&home);
        append(crate::ahjo::EV_REMOVED, json!({ "name": "work-7" }));
        // The item landed after its run in `work-7`: the branch holds landed work.
        append(EV_LANDED, json!({ "repo": "o/a", "number": 7 }));
        fresh(&home);
        // Reopened, it ran in `work-7` again, but another repository's #7 ran there since.
        append(EV_STARTED, started("o/a"));
        append(EV_STARTED, started("o/fork"));
        fresh(&home);
        // Its own run there is the latest again, after the landing: the branch is its.
        append(EV_STARTED, started("o/a"));
        let (env, resumed) = super::environment(&home, &a, "HEAD", &item).unwrap();
        assert!(resumed);
        assert_eq!(env.name, item.env_name());
    }

    #[test]
    fn a_run_that_errors_frees_only_an_environment_that_is_its_and_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let (a, b) = (
            repo_with_commit(tmp.path(), "a"),
            repo_with_commit(tmp.path(), "b"),
        );
        let item = work_item("o/a", 7);
        let create = |name: &str, repo: &std::path::Path| {
            crate::ahjo::create(
                &home,
                crate::ahjo::CreateOptions {
                    name,
                    repo,
                    base: None,
                },
            )
            .unwrap()
        };
        // A `work-7` with an edit an older release's park kept stays, edit and all.
        let old = create("work-7", &a);
        std::fs::write(old.path.join("draft.txt"), "the builder's edit\n").unwrap();
        free_after_error(&home, &a, &item);
        assert!(old.path.join("draft.txt").exists());
        assert!(crate::ahjo::get(&home, "work-7").is_ok());
        // Clean but off its branch (commits on a detached HEAD would be lost): it stays too.
        std::fs::remove_file(old.path.join("draft.txt")).unwrap();
        git(&old.path, &["checkout", "-q", "--detach"]).unwrap();
        free_after_error(&home, &a, &item);
        assert!(crate::ahjo::get(&home, "work-7").is_ok());
        // Once clean and on its branch it goes, and its branch stays.
        git(&old.path, &["checkout", "-q", "paja/work-7"]).unwrap();
        free_after_error(&home, &a, &item);
        assert!(crate::ahjo::get(&home, "work-7").is_err());
        assert!(!old.path.exists());
        git(
            &a,
            &["rev-parse", "--verify", "--quiet", "refs/heads/paja/work-7"],
        )
        .unwrap();
        // The item's name held by another repository's environment: that one stays.
        let theirs = create(&item.env_name(), &b);
        free_after_error(&home, &a, &item);
        assert!(theirs.path.exists());
        assert_eq!(
            crate::ahjo::get(&home, &item.env_name()).unwrap().repo,
            theirs.repo
        );
        // The item's own is removed, uncommitted edits too, as before.
        crate::ahjo::remove(&home, &theirs.name, true).unwrap();
        let own = create(&item.env_name(), &a);
        std::fs::write(own.path.join("draft.txt"), "edit\n").unwrap();
        free_after_error(&home, &a, &item);
        assert!(crate::ahjo::get(&home, &own.name).is_err());
    }

    #[test]
    fn an_items_own_branch_wins_over_an_old_work_n_one() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let a = repo_with_commit(tmp.path(), "a");
        let item = work_item("o/a", 7);
        git(&a, &["branch", "paja/work-7"]).unwrap();
        let own = crate::ahjo::create(
            &home,
            crate::ahjo::CreateOptions {
                name: &item.env_name(),
                repo: &a,
                base: None,
            },
        )
        .unwrap();
        crate::ahjo::remove(&home, &own.name, true).unwrap();
        let (env, resumed) = super::environment(&home, &a, "HEAD", &item).unwrap();
        assert!(resumed);
        assert_eq!(env.branch, own.branch);
        // The old one is left alone.
        git(
            &a,
            &["rev-parse", "--verify", "--quiet", "refs/heads/paja/work-7"],
        )
        .unwrap();
    }

    #[test]
    fn a_work_n_environment_whose_repository_moved_is_neither_taken_nor_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let home = crate::home::Home::at(tmp.path().join("home")).unwrap();
        let a = repo_with_commit(tmp.path(), "a");
        // Recorded for a repository path that no longer exists, so it can't be canonicalized.
        let gone = tmp.path().join("moved-away");
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                crate::ahjo::EV_CREATED,
                json!({
                    "name": "work-7", "repo": gone, "path": gone.join("w"), "branch": "paja/work-7",
                    "base_commit": "abc", "port_base": 20_000, "port_count": 100, "created_ms": 1
                }),
            )
            .unwrap();
        drop(lock);
        let item = work_item("o/a", 7);
        assert!(existing_environment(&home, &a, &item).unwrap().is_none());
        let (env, resumed) = super::environment(&home, &a, "HEAD", &item).unwrap();
        assert!(!resumed);
        assert_eq!(env.name, item.env_name());
        assert_eq!(crate::ahjo::get(&home, "work-7").unwrap().repo, gone);
    }

    #[test]
    fn reset_times_are_read_and_shown_in_utc() {
        // 2026-11-27 18:30:00 UTC.
        assert_eq!(super::format_utc(1_795_804_200_000), "2026-11-27 18:30 UTC");
        assert_eq!(super::format_utc(0), "1970-01-01 00:00 UTC");
        let limited = serde_json::json!({"result": "Claude usage limit reached|1795804200"});
        assert_eq!(
            super::reset_time(Some(&limited), ""),
            Some(1_795_804_200_000)
        );
        let field = serde_json::json!({"resets_at": 1_795_804_200_000u64});
        assert_eq!(super::reset_time(Some(&field), ""), Some(1_795_804_200_000));
        // No time, or a stray number that is no epoch: unknown.
        assert_eq!(super::reset_time(None, "limit reached|42"), None);
        assert_eq!(super::reset_time(None, "limit reached"), None);
    }

    #[test]
    fn a_whole_failed_result_is_a_usage_limit_with_its_reset_time() {
        // The shape of a failed `claude -p --output-format json` result at the limit.
        let result = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": true, "api_error_status": 429,
            "result": "Claude AI usage limit reached|1795804200", "session_id": "s-1",
            "total_cost_usd": 0.3,
        });
        let err = anyhow::Error::new(super::ClaudeFailed(
            result["result"].as_str().unwrap().to_string(),
            Some(result),
        ));
        let limit = super::usage_limit(&err).expect("a 429 is a usage limit");
        assert_eq!(limit.resets_at, Some(1_795_804_200_000));
        // Without a time in the text it is still a limit, with no reset time.
        let result = serde_json::json!({"is_error": true, "api_error_status": 429,
                                        "result": "API Error: Rate limit reached"});
        let err = anyhow::Error::new(super::ClaudeFailed("rate limit".into(), Some(result)));
        assert_eq!(super::usage_limit(&err).unwrap().resets_at, None);
        // Other failures are not limits.
        let result = serde_json::json!({"is_error": true, "api_error_status": 500});
        let err = anyhow::Error::new(super::ClaudeFailed("boom".into(), Some(result)));
        assert!(super::usage_limit(&err).is_none());
    }

    #[test]
    fn sandbox_and_platform_are_high_risk() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.paja/high-risk"),
        )
        .unwrap();
        let prefixes = super::lines(&text);
        for file in ["crates/paja/src/sandbox.rs", "crates/paja/src/platform.rs"] {
            assert!(super::is_high_risk(file, &prefixes), "{file}");
        }
    }

    #[test]
    fn the_high_risk_mestari_files_are_listed_and_the_rest_are_not() {
        let text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.paja/high-risk"),
        )
        .unwrap();
        let prefixes = super::lines(&text);
        for file in ["agent.rs", "land.rs", "mod.rs", "plan.rs"] {
            let path = format!("crates/paja/src/mestari/{file}");
            assert!(super::is_high_risk(&path, &prefixes), "{path}");
        }
        for file in ["usage.rs", "progress.rs", "follow_ups.rs", "builder.rs"] {
            let path = format!("crates/paja/src/mestari/{file}");
            assert!(!super::is_high_risk(&path, &prefixes), "{path}");
        }
    }

    #[test]
    fn nested_configuration_is_high_risk() {
        let prefixes: Vec<String> = [".cargo/", "CLAUDE.md", "crates/paja/src/hook.rs"]
            .map(String::from)
            .to_vec();
        assert!(super::is_high_risk(
            "crates/x/.Cargo/config.toml",
            &prefixes
        ));
        assert!(super::is_high_risk("docs/CLAUDE.md", &prefixes));
        let names: Vec<String> = ["rust-toolchain", ".paja/"].map(String::from).to_vec();
        assert!(super::is_high_risk("rust-toolchain.toml", &names));
        let dir = std::env::temp_dir().join(format!("paja-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(
            &file,
            "[core]\n\tbare = false\n[branch \"x\"]\n\tremote = origin\n",
        )
        .unwrap();
        let plain = super::config_entries(&file).unwrap();
        std::fs::write(&file, "[core]\n\tbare = false\n").unwrap();
        assert_eq!(super::config_entries(&file).unwrap(), plain);
        // A section started on the same line as a branch header still counts.
        std::fs::write(
            &file,
            "[core]\n\tbare = false\n[branch \"x\"][credential]\n\thelper = y\n",
        )
        .unwrap();
        assert_ne!(super::config_entries(&file).unwrap(), plain);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!super::is_high_risk("src/.pajarc", &names));
        assert!(super::is_high_risk("crates/paja/src/hook.rs", &prefixes));
        assert!(!super::is_high_risk("crates/paja/src/jono.rs", &prefixes));
        assert!(!super::is_high_risk(
            "other/crates/paja/src/hook.rs",
            &prefixes
        ));
    }

    use super::*;

    #[test]
    fn the_ci_file_names_a_mode_and_a_wait() {
        let ok = |text: &str| parse_ci(text).unwrap();
        assert_eq!(ok(""), (CiMode::Required, CI_WAIT_MINUTES));
        assert_eq!(ok("# CI\nrequired\n"), (CiMode::Required, CI_WAIT_MINUTES));
        assert_eq!(ok("none\n"), (CiMode::None, CI_WAIT_MINUTES));
        assert_eq!(ok("required\nwait 30\n"), (CiMode::Required, 30));
        // `wait` alone means `required`, the default; the order of the lines doesn't matter.
        assert_eq!(ok("wait 1440\n"), (CiMode::Required, 1440));
        assert_eq!(ok("wait  45\nrequired\n"), (CiMode::Required, 45));
        let err = |text: &str| parse_ci(text).unwrap_err();
        for bad in ["wait 0", "wait 1441", "wait -5", "wait 1.5", "wait soon"] {
            assert!(
                err(bad).contains("whole number of minutes from 1 to 1440"),
                "{bad}"
            );
        }
        for bad in ["wait", "wait 5 minutes", "optional", "required now"] {
            assert!(
                err(bad).contains("is not one of them"),
                "{bad}: {}",
                err(bad)
            );
        }
        for bad in ["required\nrequired", "required\nnone", "none\nrequired"] {
            let why = err(bad);
            assert!(
                why.contains("after an earlier `required` or `none` line; keep one"),
                "{why}"
            );
        }
        let why = err("wait 5\nwait 6");
        assert!(
            why.starts_with("has `wait 6` after an earlier `wait` line"),
            "{why}"
        );
        assert!(err("none\nwait 5").contains("only `required` has"));
        assert!(err("wait 5\nnone").contains("only `required` has"));
        let branch = CiMode::parse(Some("none\nwait 5\n"), "trunk").unwrap_err();
        assert!(
            format!("{branch}").starts_with(".paja/ci on trunk says `none`")
                && format!("{branch}").ends_with("fix it on trunk"),
            "{branch}"
        );
    }

    #[test]
    fn lines_skip_blanks_and_comments() {
        assert_eq!(
            lines("# checks\ncargo test\n\n  cargo fmt --all --check \n"),
            ["cargo test", "cargo fmt --all --check"]
        );
    }

    #[test]
    fn clip_keeps_the_end_on_a_char_boundary() {
        let long = "é".repeat(3000);
        let clipped = clip(&long);
        assert!(clipped.starts_with('…') && clipped.len() <= 4004);
    }

    #[test]
    fn models_file_names_a_model_per_role() {
        use super::Models;
        let models =
            Models::parse("# comment\n\nbuild sonnet\nreview opus high\nfallback sonnet\n")
                .unwrap();
        assert_eq!(
            models.args("build"),
            ["--max-turns", "80", "--model", "sonnet"]
        );
        assert_eq!(
            models.args("review"),
            [
                "--max-turns",
                "30",
                "--model",
                "opus",
                "--effort",
                "high",
                "--fallback-model",
                "sonnet"
            ]
        );
        // A role with no line keeps the account's default model, but never runs uncapped.
        assert_eq!(models.args("fix"), ["--max-turns", "80"]);
        let capped = Models::parse("max-turns build 5\nmax-turns review-2 7\n").unwrap();
        assert_eq!(capped.max_turns("build"), 5);
        assert_eq!(capped.max_turns("review-2"), 7);
        assert_eq!(capped.max_turns("re-review"), 30);
        assert_eq!(Models::parse("").unwrap(), Models::default());
        // A role this Paja doesn't know (one a newer Paja added, or a typo) is skipped, so a
        // runner whose binary is behind main keeps going; the other lines still count.
        let ahead =
            Models::parse("reviewer opus\nbuild sonnet\nmax-turns builder 3\nmax-turns fix 9\n")
                .unwrap();
        assert_eq!(ahead.roles.keys().collect::<Vec<_>>(), ["build"]);
        assert_eq!(ahead.max_turns.keys().collect::<Vec<_>>(), ["fix"]);
        assert_eq!(ahead.skipped, ["reviewer", "builder"]);
        // A well-formed `max-turns` line for an unknown role, alone, is skipped too.
        let alone = Models::parse("max-turns builder 3\n").unwrap();
        assert!(alone.max_turns.is_empty(), "{alone:?}");
        assert_eq!(alone.skipped, ["builder"]);
        // Its model and effort are still checked: a skipped line can't hide a flag.
        let err = format!("{:#}", Models::parse("newrole --x").unwrap_err());
        assert!(err.contains("not a plain model or effort name"), "{err}");
        for (text, says) in [
            ("build", "write it as `build <model> [effort]`"),
            (
                "build sonnet high extra",
                "write it as `build <model> [effort]`",
            ),
            ("build sonnet loud", "unknown effort \"loud\""),
            ("build sonnet\nbuild opus", "line 2"),
            ("fallback", "write it as `fallback <model>`"),
            ("fallback a\nfallback b", "`fallback` is named twice"),
            ("max-turns build", "write it as `max-turns <role> <turns>`"),
            (
                "max-turns build many",
                "write it as `max-turns <role> <turns>`",
            ),
            ("max-turns build 0", "at least 1"),
            ("max-turns fix 3\nmax-turns fix 4", "named twice"),
            ("build $(x)", "not a plain model or effort name"),
            (
                "build --dangerously-skip-permissions",
                "not a plain model or effort name",
            ),
        ] {
            let err = format!("{:#}", Models::parse(text).unwrap_err());
            assert!(err.contains(".paja/models on main"), "{err}");
            assert!(err.contains(says), "{text:?}: {err}");
        }
    }

    #[test]
    fn each_review_takes_its_role() {
        use super::review_role;
        assert_eq!(review_role(false, 0, false), "review");
        assert_eq!(review_role(false, 0, true), "re-review");
        assert_eq!(review_role(true, 0, false), "review");
        assert_eq!(review_role(true, 1, false), "review-2");
        // High-risk reviewers read the whole change every round, so never `re-review`.
        assert_eq!(review_role(true, 0, true), "review");
        assert_eq!(review_role(true, 1, true), "review-2");
    }

    #[test]
    fn the_review_prompt_names_the_checked_commit_and_skips_the_suite() {
        let item = crate::jono::WorkItem {
            repo: "acme/app".into(),
            number: 9,
            title: "T".into(),
            body: "B".into(),
            url: String::new(),
            open: true,
            status: crate::jono::Status::Active,
            note: None,
            usage: None,
            answers: Vec::new(),
            waiting_on: Vec::new(),
            priority: Default::default(),
            claim: None,
        };
        let checks: Vec<String> = ["cargo fmt --all --check", "cargo test"]
            .map(String::from)
            .to_vec();
        let cargo = vec!["cargo".to_string()];
        let review_prompt = |item: &_, prior: Option<&super::Prior>| {
            super::review_prompt(
                item,
                "base1",
                "abc123",
                prior,
                &[],
                "DIFF-TEXT",
                &checks,
                &cargo,
            )
        };
        let prompt = review_prompt(&item, None);
        assert!(
            prompt.contains("`cargo fmt --all --check`, `cargo test`"),
            "{prompt}"
        );
        assert!(
            prompt.contains("Bash limited to local git and `cargo`"),
            "{prompt}"
        );
        let builder = super::builder_prompt(&item, &cargo, &checks);
        assert!(builder.contains("only for local git and `cargo`"));
        assert!(builder.contains("`cargo fmt --all --check`, `cargo test`"));
        // No review programs: the reviewer is not told to run anything.
        let bare = super::review_prompt(&item, "base1", "abc123", None, &[], "D", &checks, &[]);
        assert!(!bare.contains("run one named test"), "{bare}");
        assert!(bare.contains("limited to local git"), "{bare}");
        assert!(bare.contains("Read-only git is fine"), "{bare}");

        // Another project: neither prompt mentions cargo or Paja as the project.
        let npm = vec!["npm".to_string(), "node".to_string()];
        let js = super::review_prompt(
            &item,
            "base1",
            "abc123",
            None,
            &[],
            "DIFF-TEXT",
            &["npm test".to_string()],
            &npm,
        );
        let build = super::builder_prompt(&item, &npm, &["npm test".to_string()]);
        for text in [&js, &build] {
            assert!(!text.contains("cargo"), "{text}");
            assert!(!text.contains("reviewer for Paja"), "{text}");
        }
        assert!(js.contains("`npm test`"), "{js}");
        assert!(js.contains("local git, `npm` and `node`"), "{js}");
        assert!(
            build.contains("only for local git, `npm` and `node`"),
            "{build}"
        );
        assert_eq!(super::program_tools(&npm), ["Bash(npm *)", "Bash(node *)"]);

        assert!(prompt.contains("DIFF-TEXT"), "{prompt}");
        assert!(!prompt.contains("git show base1:CLAUDE.md"), "{prompt}");
        assert!(prompt.contains("CLAUDE.md in your system prompt is the default branch's copy"));
        assert!(prompt.find("Issue #9").unwrap() < prompt.find("independent reviewer").unwrap());
        assert!(prompt.contains("already passed at abc123"), "{prompt}");
        // A re-review judges only the fix, at the checked commit, against the earlier blockers.
        let prior = super::Prior {
            reviewed: "old456".into(),
            blockers: "Fix the parser.".into(),
            follow_ups: vec![Finding {
                kind: FindingKind::Risk,
                text: "Tidy the helper".into(),
            }],
        };
        let prompt = review_prompt(&item, Some(&prior));
        assert!(prompt.contains("already passed at abc123"), "{prompt}");
        assert!(prompt.contains("`git diff old456..HEAD`"), "{prompt}");
        assert!(prompt.contains("Fix the parser."), "{prompt}");
        assert!(prompt.contains("Tidy the helper"), "{prompt}");
        assert!(!prompt.contains("git diff base1...HEAD"), "{prompt}");
    }

    #[test]
    fn the_review_diff_holds_the_stat_and_names_files_past_the_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("keep.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "base"]);
        run(&["tag", "base"]);
        std::fs::write(dir.join("a.txt"), "small-change\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x".repeat(500)).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "change"]);
        let all = super::review_diff(dir, "base...HEAD", 60_000).unwrap();
        assert!(all.contains("a.txt |"), "{all}");
        assert!(all.contains("small-change"), "{all}");
        assert!(!all.contains("size cap"), "{all}");
        let capped = super::review_diff(dir, "base...HEAD", 300).unwrap();
        assert!(capped.contains("a.txt |"), "{capped}");
        assert!(capped.contains("small-change"), "{capped}");
        assert!(!capped.contains("xxxxxxxx"), "{capped}");
        assert!(
            capped.contains("you must read these files yourself"),
            "{capped}"
        );
        assert!(capped.contains("b.txt"), "{capped}");
        assert!(capped.contains("b.txt ("), "{capped}");
    }

    #[test]
    fn the_review_diff_shows_the_head_of_a_first_file_over_the_cap_and_reads_names_from_git() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("keep.txt"), "one\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "base"]);
        run(&["tag", "base"]);
        // A first file (in path order) far over the cap, then one whose path has ` b/` in it.
        std::fs::create_dir(dir.join("x b")).unwrap();
        std::fs::write(dir.join("a.txt"), "y".repeat(900)).unwrap();
        std::fs::write(dir.join("x b").join("z.txt"), "later\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "change"]);
        let capped = super::review_diff(dir, "base...HEAD", 300).unwrap();
        assert!(
            capped.contains("The first file, a.txt, is over the cap"),
            "{capped}"
        );
        assert!(capped.contains("yyyyyyyy"), "{capped}");
        assert!(!capped.contains(&"y".repeat(400)), "{capped}");
        assert!(capped.contains("x b/z.txt ("), "{capped}");
        assert!(!capped.contains("(unknown file)"), "{capped}");
    }

    #[test]
    fn the_review_diff_names_renamed_mode_only_and_binary_files_past_the_cap() {
        // The skipped files' names come from `git diff --name-only` and their sections from
        // splitting the diff, matched by position: a rename, a mode-only change and a binary file
        // each make one section, so every name keeps its own line count.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        run(&["init", "-q"]);
        std::fs::write(dir.join("m.sh"), "echo hi\n").unwrap();
        std::fs::write(dir.join("old.txt"), "rename me\n".repeat(20)).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "base"]);
        run(&["tag", "base"]);
        std::fs::write(dir.join("a.txt"), "y".repeat(900)).unwrap();
        std::fs::write(dir.join("c.bin"), [0u8, 1, 2, 0, 255, 0]).unwrap();
        std::fs::write(dir.join("z.txt"), "one\ntwo\n").unwrap();
        run(&["mv", "old.txt", "new.txt"]);
        // A rename with one line changed, so its section differs in length from the binary
        // file's; and renames off in the repository's configuration, which Paja's own diff
        // pins back on.
        let mut text = "rename me\n".repeat(19);
        text.push_str("changed\n");
        std::fs::write(dir.join("new.txt"), text).unwrap();
        run(&["add", "-A"]);
        // Last, as `git add` would read the file's mode back from the disk.
        run(&["update-index", "--chmod=+x", "m.sh"]);
        run(&["config", "diff.renames", "false"]);
        run(&["commit", "-q", "-m", "change"]);
        let capped = super::review_diff(dir, "base...HEAD", 300).unwrap();
        assert!(
            capped.contains("The first file, a.txt, is over the cap"),
            "{capped}"
        );
        // Header, mode and index lines plus `Binary files ... differ`; old and new mode; the
        // similarity, rename and index lines with the hunk of one changed line; a new file's
        // five header lines and its two added lines.
        for skipped in [
            "c.bin (4 lines)",
            "m.sh (3 lines)",
            "new.txt (13 lines)",
            "z.txt (8 lines)",
        ] {
            assert!(capped.contains(skipped), "{skipped}: {capped}");
        }
        assert!(!capped.contains("(unknown file)"), "{capped}");
    }

    #[test]
    fn diff_sections_are_named_by_their_header_when_git_lists_other_names() {
        let files = vec![
            "diff --git a/a.txt b/a.txt\n+x\n".to_string(),
            "diff --git a/b.txt b/b.txt\n+y\n".to_string(),
        ];
        assert_eq!(
            super::section_names(&["a.txt", "b.txt"], &files),
            ["a.txt", "b.txt"]
        );
        assert_eq!(
            super::section_names(&["b.txt"], &files),
            ["a/a.txt b/a.txt", "a/b.txt b/b.txt"]
        );
    }

    #[test]
    fn the_prompts_fit_this_repositorys_own_checks_and_commands() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let checks = super::lines(&std::fs::read_to_string(root.join(".paja/checks")).unwrap());
        let commands = crate::hook::Commands::parse(
            std::fs::read_to_string(root.join(".paja/commands"))
                .ok()
                .as_deref(),
        )
        .unwrap();
        assert!(!checks.is_empty());
        let item = crate::jono::WorkItem {
            repo: "acme/app".into(),
            number: 9,
            title: "T".into(),
            body: "B".into(),
            url: String::new(),
            open: true,
            status: crate::jono::Status::Active,
            note: None,
            usage: None,
            answers: Vec::new(),
            waiting_on: Vec::new(),
            priority: Default::default(),
            claim: None,
        };
        let review = super::review_prompt(
            &item,
            "base1",
            "abc123",
            None,
            &[],
            "D",
            &checks,
            &commands.reviewer,
        );
        let build = super::builder_prompt(&item, &commands.builder, &checks);
        for check in &checks {
            assert!(review.contains(&format!("`{check}`")), "{check}: {review}");
            assert!(build.contains(&format!("`{check}`")), "{check}: {build}");
        }
        for program in &commands.builder {
            assert!(
                build.contains(&format!("`{program}`")),
                "{program}: {build}"
            );
        }
        for program in &commands.reviewer {
            assert!(
                review.contains(&format!("`{program}`")),
                "{program}: {review}"
            );
        }
    }

    #[test]
    fn every_source_file_is_high_risk_or_reviewed_as_standard_risk_on_purpose() {
        // A new module has to be placed in `.paja/high-risk` or in this list, so it can't land
        // at standard risk by being forgotten.
        // Paths are relative to `crates/paja/src`, and every directory under it is read.
        const STANDARD: &[&str] = &[
            "doctor.rs",
            "home.rs",
            "init.rs",
            // Checks the init agent proposes for another repository, kept only when files back
            // them; this repository's own `.paja/` is high risk.
            "init/detect.rs",
            "init/recommend.rs",
            // Pushes the first commit of a brand-new repository, not a landing.
            "init/scaffold.rs",
            "jono.rs",
            "mcp.rs",
            "mestari/builder.rs",
            "mestari/follow_ups.rs",
            "mestari/live.rs",
            "mestari/pool.rs",
            "mestari/progress.rs",
            "mestari/usage.rs",
            "posti.rs",
            "usage.rs",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let prefixes =
            super::lines(&std::fs::read_to_string(root.join(".paja/high-risk")).unwrap());
        let src = root.join("crates/paja/src");
        let mut dirs = vec![src.clone()];
        let mut seen = 0;
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                // Always `/`, as `.paja/high-risk` writes paths, on Windows too.
                let relative = path
                    .strip_prefix(&src)
                    .unwrap()
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let full = format!("crates/paja/src/{relative}");
                assert!(
                    super::is_high_risk(&full, &prefixes) || STANDARD.contains(&relative.as_str()),
                    "{full} is neither in .paja/high-risk nor in this test's standard-risk list; \
                     decide which it is"
                );
                seen += 1;
            }
        }
        // The walk reached the subdirectories, where most of Mestari's code is.
        assert!(
            seen > 25,
            "only {seen} source files found under {}",
            src.display()
        );
        // Each listed file still exists, so the list can't hide a rename.
        for name in STANDARD {
            assert!(
                src.join(name).is_file(),
                "{name} is listed as standard risk but is gone"
            );
        }
    }

    /// The events of item 7 of `o/r`, numbered in order.
    fn item_log(events: &[(&str, serde_json::Value)]) -> Vec<crate::events::Event> {
        events
            .iter()
            .enumerate()
            .map(|(i, (kind, data))| {
                let mut data = data.clone();
                data["repo"] = "o/r".into();
                data["number"] = 7.into();
                crate::events::Event {
                    v: 1,
                    seq: i as u64 + 1,
                    ts_ms: 0,
                    kind: kind.to_string(),
                    data,
                }
            })
            .collect()
    }

    fn ended(round: u32) -> (&'static str, serde_json::Value) {
        (
            EV_ROUND,
            serde_json::json!({"round": round, "verdict": "changes", "next": {"feedback": "fix it"}}),
        )
    }

    #[test]
    fn resume_point_counts_rounds_since_the_item_was_last_queued() {
        use serde_json::json;
        let at = |events: &[(&str, serde_json::Value)]| {
            let point = super::resume_point(&item_log(events), "o/r", 7);
            (point.round, point.feedback, point.checked)
        };
        // Nothing yet: round 1.
        assert_eq!(at(&[]), (1, String::new(), None));
        // A round that ended with a verdict moves on to the next one, with its feedback.
        assert_eq!(at(&[ended(1)]), (2, "fix it".into(), None));
        // A round without `next` was cut off: it is run again, not moved past.
        let cut = (EV_ROUND, json!({"round": 2}));
        assert_eq!(at(&[ended(1), cut.clone()]).0, 2);
        assert_eq!(at(&[cut]).0, 2);
        let checked = |round: u32| {
            (
                EV_CHECKED,
                json!({"round": round, "commit": "c1", "base": "b1"}),
            )
        };
        // Each event that starts the item afresh resets the count, and drops the checks that
        // round 1 of the next cycle would otherwise take for its own.
        for kind in [
            jono::EV_STATUS,
            jono::EV_ANSWERED,
            jono::EV_REOPENED,
            EV_LANDED,
            EV_ESCALATED,
        ] {
            assert_eq!(
                at(&[ended(1), ended(2), checked(3), (kind, json!({}))]),
                (1, String::new(), None),
                "{kind} resets the round count"
            );
        }
        // Another item's rounds don't count.
        let mut other = item_log(&[ended(1), ended(2)]);
        for event in &mut other {
            event.data["number"] = 8.into();
        }
        assert_eq!(super::resume_point(&other, "o/r", 7).round, 1);
        // Checks count only for the round they ran in.
        assert_eq!(
            at(&[ended(1), checked(2)]).2,
            Some(("c1".into(), "b1".into()))
        );
        assert_eq!(at(&[ended(1), checked(1)]).2, None);
        // Checks from a round that later ended don't carry into the next one.
        assert_eq!(at(&[checked(1), ended(1)]).2, None);
        // What a review noted outside the change carries into the round after a conflict at the
        // turn to land, and no further.
        let noted = (
            EV_ROUND,
            json!({"round": 1, "verdict": "conflict", "next": {"feedback": "merge",
                "noted": [{"kind": "outside", "finding": "The parser drops a line"}]}}),
        );
        let point = super::resume_point(&item_log(std::slice::from_ref(&noted)), "o/r", 7);
        assert_eq!(point.noted.len(), 1);
        assert_eq!(point.noted[0].text, "The parser drops a line");
        let point = super::resume_point(&item_log(&[noted.clone(), ended(2)]), "o/r", 7);
        assert!(point.noted.is_empty());
        // A round that ended approved, cut off at the turn to land, names the latest ones, which
        // replace what the round before it handed on, prior or not.
        let approved = (
            EV_ROUND,
            json!({"round": 2, "verdict": "land",
                "noted": [{"kind": "outside", "finding": "The lexer drops a byte"}]}),
        );
        let point = super::resume_point(&item_log(&[noted.clone(), approved.clone()]), "o/r", 7);
        assert_eq!(point.round, 2);
        assert_eq!(point.noted[0].text, "The lexer drops a byte");
        let with_prior = (
            EV_ROUND,
            json!({"round": 1, "verdict": "changes", "next": {"feedback": "fix it",
                "prior": {"reviewed": "r1", "blockers": "b", "follow_ups": []}}}),
        );
        let point = super::resume_point(&item_log(&[with_prior.clone(), approved]), "o/r", 7);
        assert_eq!(
            point.prior.unwrap().follow_ups[0].text,
            "The lexer drops a byte"
        );
        // An approved round that noted none clears the earlier ones; a stopped round, which
        // records no `noted`, keeps them.
        let none = (
            EV_ROUND,
            json!({"round": 2, "verdict": "land", "noted": []}),
        );
        let point = super::resume_point(&item_log(&[noted.clone(), none.clone()]), "o/r", 7);
        assert!(point.noted.is_empty());
        let point = super::resume_point(&item_log(&[with_prior, none]), "o/r", 7);
        assert!(point.prior.unwrap().follow_ups.is_empty());
        let stopped = (EV_ROUND, json!({"round": 2, "verdict": "stopped"}));
        let point = super::resume_point(&item_log(&[noted, stopped.clone()]), "o/r", 7);
        assert_eq!(point.noted.len(), 1);
        // A round every reviewer approved keeps its commit and the findings it lands with, in
        // the change and outside it, through a stop. Checks on Paja's merge at the turn to land
        // move it to that commit; checks on any other commit, a round that went on, or a fresh
        // start take it back; a round that only noted findings (a split, an escalation) never
        // gave one.
        let checked_as = |commit: &str, landing: bool| {
            (
                EV_CHECKED,
                json!({"round": 5, "commit": commit, "base": "b1", "landing": landing}),
            )
        };
        let landing = (
            EV_ROUND,
            json!({"round": 5, "verdict": "land,land", "noted": [],
                "approved": {"reviewed": "c1", "commit": "c1", "reviewers": 2, "findings": [
                    {"kind": "nit", "finding": "Rename x"},
                    {"kind": "outside", "finding": "The lexer drops a byte"}]}}),
        );
        let resumed =
            |events: &[(&str, serde_json::Value)]| super::resume_point(&item_log(events), "o/r", 7);
        let point = resumed(&[
            checked_as("c1", false),
            landing.clone(),
            checked_as("m1", true),
            stopped.clone(),
        ]);
        let approval = point.approved.expect("approved");
        assert_eq!(approval.findings.len(), 2);
        assert_eq!(approval.findings[0].text, "Rename x");
        assert_eq!(approval.reviewers, 2);
        assert_eq!(
            (approval.reviewed.as_str(), approval.commit.as_str()),
            ("c1", "m1")
        );
        assert_eq!(point.round, 5);
        assert_eq!(point.checked, Some(("m1".into(), "b1".into())));
        // A rebuild after the approval, checked and then stopped in its review.
        let point = resumed(&[
            checked_as("c1", false),
            landing.clone(),
            checked_as("x1", false),
            stopped.clone(),
        ]);
        assert!(point.approved.is_none());
        // The same commit checked again (the default branch tip moved, nothing merged) keeps it.
        let point = resumed(&[landing.clone(), checked_as("c1", false)]);
        assert!(point.approved.is_some());
        let red = (
            EV_ROUND,
            json!({"round": 5, "verdict": "ci_failed", "next": {"feedback": "red"}}),
        );
        let after = |event: (&str, serde_json::Value)| resumed(&[landing.clone(), event]).approved;
        assert!(after(red).is_none());
        assert!(after((jono::EV_STATUS, json!({}))).is_none());
        assert!(after(stopped).is_some());
        let split = (
            EV_ROUND,
            json!({"round": 5, "verdict": "land,changes", "noted": []}),
        );
        assert!(resumed(std::slice::from_ref(&split)).approved.is_none());
        // A later review of the same round that didn't approve takes the approval back.
        let escalated = (
            EV_ROUND,
            json!({"round": 5, "verdict": "escalate", "noted": []}),
        );
        assert!(after(escalated).is_none());
        assert!(after(split).is_none());
        // An older approval shape, or none, is no approval.
        let older = (
            EV_ROUND,
            json!({"round": 5, "verdict": "land", "noted": [], "approved": []}),
        );
        assert!(resumed(&[older]).approved.is_none());
    }

    #[test]
    fn a_full_review_hears_what_an_earlier_review_noted_outside_the_change() {
        let item = WorkItem {
            repo: "o/r".into(),
            number: 7,
            title: "Add a parser".into(),
            body: "Please.".into(),
            url: String::new(),
            open: true,
            status: crate::jono::Status::Active,
            note: None,
            usage: None,
            answers: Vec::new(),
            waiting_on: Vec::new(),
            priority: Default::default(),
            claim: None,
        };
        let noted = [Finding {
            kind: FindingKind::Outside,
            text: "The parser drops a line".into(),
        }];
        let prompt = super::review_prompt(&item, "base1", "abc123", None, &noted, "D", &[], &[]);
        assert!(
            prompt.contains("noted these findings outside the change")
                && prompt.contains("The parser drops a line"),
            "{prompt}"
        );
        let bare = super::review_prompt(&item, "base1", "abc123", None, &[], "D", &[], &[]);
        assert!(!bare.contains("noted these findings"), "{bare}");
    }
}
