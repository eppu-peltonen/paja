//! Mestari: runs one work item through its graph. The first graph is the smallest loop that
//! can build Paja: build in a fresh Ahjo environment, run Vasara's checks, have independent
//! reviewer agents read the diff cold, then land it or loop back, and escalate when stuck.
//!
//! Every step is a fresh headless `claude -p` run. The checks and the high-risk paths are read
//! from the base commit on `main`, never from the branch under review, so a change can't
//! loosen the checks that judge it.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::ahjo::{self, CreateOptions, Env};
use crate::home::Home;
use crate::jono::{self, Status, WorkItem};

mod agent;
mod builder;
mod follow_ups;
mod land;
mod progress;
mod usage;

use agent::*;
use builder::*;
use follow_ups::*;
use land::*;
use usage::*;

pub use progress::progress;
use progress::*;
pub use usage::{ModelUsage, StepUsage};

/// Most build rounds before a run escalates instead of trying again.
pub const MAX_ROUNDS: u32 = 3;
/// Vasara's checks: one shell command per line, read from the base commit.
pub const CHECKS_FILE: &str = ".paja/checks";
/// Path prefixes that make a change high risk (two reviewers), read from the base commit.
pub const HIGH_RISK_FILE: &str = ".paja/high-risk";
/// The model and effort for each agent role, read from the base commit.
pub const MODELS_FILE: &str = ".paja/models";
pub const CI_FILE: &str = ".paja/ci";
/// The roles `.paja/models` may name, besides `fallback`.
pub const ROLES: [&str; 5] = ["build", "fix", "review", "re-review", "review-2"];
/// The effort levels Claude Code takes.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

pub const EV_STARTED: &str = "run.started";
pub const EV_ROUND: &str = "run.round";
/// A builder session is about to start: its session id, recorded before it runs, so a run
/// that is killed mid-session can resume it.
pub const EV_STEP_STARTED: &str = "run.step_started";
/// A builder session's work was thrown away (the hook never ran), so it is never resumed.
pub const EV_STEP_DISCARDED: &str = "run.step_discarded";
pub const EV_LANDED: &str = "run.landed";
pub const EV_ESCALATED: &str = "run.escalated";
/// A non-blocking review finding filed as a `paja-follow-up` issue after landing.
pub const EV_FOLLOW_UP: &str = "run.follow_up";
/// The tamper lock lives in Ahjo, next to the git every command runs.
pub use crate::ahjo::{Tampered, EV_TAMPERED, EV_TAMPER_CHECKED};

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
    /// The person pressed Ctrl-C. The item stays active, and the next `paja run` resumes it in
    /// the same environment.
    Stopped { number: u64, rounds: u32 },
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
    /// Findings that don't block landing, each marked `bug`, `risk` or `nit`. Once the change
    /// lands, Mestari files its bugs and risks together as one follow-up issue, for a person to
    /// queue, instead of holding this change up; nits are never filed.
    #[serde(default)]
    pub follow_ups: Vec<Finding>,
}

/// The model, and optionally the effort, one role runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleModel {
    pub model: String,
    pub effort: Option<String>,
}

/// `.paja/models` as it is on main: one line per role, `role model [effort]`, plus an optional
/// `fallback model` line. A role with no line runs on the account's default model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Models {
    pub roles: std::collections::BTreeMap<String, RoleModel>,
    pub fallback: Option<String>,
    /// `max-turns <role> <n>` lines: the most turns one session of the role may take.
    pub max_turns: std::collections::BTreeMap<String, u32>,
}

impl Models {
    /// Read the file's text. `#` starts a comment line. Anything else that isn't a known role
    /// with a plain model name (and a known effort) is an error naming the line and the fix.
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
                    bail!(
                        "{}: unknown role {named:?}; use one of {}",
                        at(),
                        ROLES.join(", ")
                    );
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
                bail!(
                    "{}: unknown role {role:?}; use one of {} or fallback",
                    at(),
                    ROLES.join(", ")
                );
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

    /// The flags `claude -p` gets for a role. The model flags are left out when the file
    /// doesn't name the role.
    fn args(&self, role: &str) -> Vec<String> {
        let mut args = vec!["--max-turns".to_string(), self.max_turns(role).to_string()];
        let Some(chosen) = self.roles.get(role) else {
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
        let chosen = self.roles.get(role)?;
        self.fallback.as_deref().filter(|f| *f != chosen.model)
    }
}

/// What an earlier round's reviewers asked for, so the next review judges only the fix.
struct Prior {
    /// The commit those reviewers judged.
    reviewed: String,
    /// The blockers they named.
    blockers: String,
    /// Their non-blocking findings, for the next reviewer to keep or drop.
    follow_ups: Vec<Finding>,
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

/// What a run is about to do, on stderr, before the first agent starts.
fn summary(item: &jono::WorkItem, ci: CiMode, sandbox: bool) {
    progress(&format!("repository: {}", item.repo));
    progress(&format!("item: #{} {}", item.number, item.title));
    progress("merges into: main (default branch), by squash");
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

/// Ask on stderr and read a yes or no from stdin.
fn confirmed() -> Result<bool> {
    eprint!("Go ahead? [y/N] ");
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Take the next work item and run it to an outcome.
pub fn run_once(home: &Home, opts: RunOptions<'_>) -> Result<RunReport> {
    let repo = std::path::absolute(opts.repo)?;
    // A repository whose configuration was tampered with gets no git, not even a fetch, until
    // a person says it was checked.
    if opts.tamper_checked {
        ahjo::clear_tampered(home, &repo)?;
    }
    let key = ahjo::ensure_untampered(home, &repo)?;
    // Once per pass: the default branch and the merge methods the repository allows.
    let target = RepoSettings::read(&repo)?;
    let branch = target.branch.as_str();
    git(&repo, &["fetch", "-q", "origin", branch]).with_context(|| {
        format!(
            "fetching origin/{branch}; check the network and that `origin` is the GitHub \
             repository"
        )
    })?;
    let base = git(&repo, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])?;
    // Before the queue is touched: a sandbox that can't start (or whose `.paja/setup` fails) is
    // the person's to fix, and must not park an item.
    crate::sandbox::activate(start_sandbox(home, &repo, &base)?);
    let labels =
        jono::Labels::parse(&read_from_base(&repo, &base, jono::LABELS_FILE)?.unwrap_or_default())?;
    progress(&format!(
        "checking the queue (issues labelled {})",
        labels.work
    ));
    // The work items of this checkout's repository only: other repositories' items sharing the
    // home are never claimed.
    let name = jono::sync_with(home, &repo, Some(&labels))?.repo;
    let config = read_config(&repo, &base, labels, target)?;
    // Before an item is claimed: waiting for CI that will never run fails after the whole build.
    if config.ci == CiMode::Required && run_gh(&repo, &["workflow", "list"])?.is_empty() {
        bail!(
            "this repository has no GitHub Actions workflow, so Paja would wait for CI that never \
             comes; add a workflow that runs the checks on pull requests, or put `none` in \
             {CI_FILE} on the default branch to land on Vasara's checks and the reviewers alone"
        );
    }
    // The claim's lock is held until this pass is done with the item, however it ends.
    let Some(jono::RunClaim {
        item,
        lock: _working,
        resumed,
    }) = jono::claim_for_run_item(home, &name, opts.item)?
    else {
        if let Some(number) = opts.item {
            bail!(
                "work item #{number} is not queued (or another run holds it); see \
                 `paja work list`"
            );
        }
        if let Some(waiting) = jono::waiting(home, &name)? {
            progress(&format!("nothing is ready: {waiting}"));
        }
        return Ok(RunReport::Idle);
    };
    summary(&item, config.ci, crate::sandbox::active().is_some());
    if opts.confirm && !confirmed()? {
        // Put the item back, so declining leaves the queue as it was. A resumed item was
        // active already and stays so.
        if !resumed {
            jono::set_status(home, &item.repo, item.number, Status::Queued, None)?;
        }
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
            // The steps that ran before the error were spent too; keep them in the item's usage.
            if !run.steps.is_empty() {
                let error = format!("{err:#}");
                record_round(home, &item, rounds, "error", &error, &mut run.steps)?;
            }
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
            let asked = comment(&repo, &item, &question);
            progress(&parked_line(
                &format!("#{} stopped on an error and was parked", item.number),
                item.number,
                asked.err(),
            ));
            let _ = ahjo::remove(home, &item.env_name(), true);
            Ok(RunReport::Escalated {
                number: item.number,
                question,
                rounds,
            })
        }
    }
}

/// Where a run is, so an error can still record what the round spent: the round it is in and
/// the steps that ran in it but are not yet in a `run.round` event.
#[derive(Default)]
struct Unrecorded {
    round: u32,
    steps: Vec<StepUsage>,
}

/// Whether the repository has CI Paja waits for before landing (`.paja/ci` on `main`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CiMode {
    /// Wait for the pull request's CI checks to pass (the default).
    Required,
    /// No CI: land on Vasara's checks and the reviewers alone.
    None,
}

impl CiMode {
    fn parse(text: Option<&str>) -> Result<CiMode> {
        let words = lines(text.unwrap_or_default());
        match words.as_slice() {
            [] => Ok(CiMode::Required),
            [w] if w == "required" => Ok(CiMode::Required),
            [w] if w == "none" => Ok(CiMode::None),
            _ => bail!("{CI_FILE} on main must hold `required` or `none`; fix it on main"),
        }
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
            format!("`{gh} repo view` gave no default branch; check `gh auth status`")
        })
    }

    /// A field `gh` leaves out is not taken to forbid anything: the branch is then `main` and
    /// squash merging is allowed.
    fn parse(json: &str) -> Option<Self> {
        let v: Value = serde_json::from_str(json).ok()?;
        let branch = match v["defaultBranchRef"]["name"].as_str() {
            Some(name) if !name.is_empty() => name.to_string(),
            _ if v.get("defaultBranchRef").is_none() => "main".to_string(),
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
            Some("--squash")
        } else if self.rebase {
            Some("--rebase")
        } else if self.merge_commit {
            Some("--merge")
        } else {
            None
        }
    }
}

/// Vasara's check commands, the high-risk paths and the model per role, as the default branch
/// has them.
struct Config {
    ci: CiMode,
    checks: Vec<String>,
    high_risk: Vec<String>,
    models: Models,
    commands: crate::hook::Commands,
    labels: jono::Labels,
    target: RepoSettings,
}

/// Read and check the default branch's configuration once, before an item is claimed: a
/// mistake there is one fix on that branch, so it stops the pass instead of parking every item
/// in turn.
fn read_config(
    repo: &Path,
    base: &str,
    labels: jono::Labels,
    target: RepoSettings,
) -> Result<Config> {
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
    let ci = CiMode::parse(read_from_base(repo, base, CI_FILE)?.as_deref())?;
    Ok(Config {
        ci,
        checks,
        high_risk,
        models,
        commands,
        labels,
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
        checks,
        high_risk,
        models,
        commands,
        labels,
        target,
    } = config;
    let (env, kept) = environment(home, repo, base, item)?;
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
        json!({ "repo": item.repo, "number": item.number, "env": env.name, "base": base }),
    )?;

    let mut feedback = String::new();
    let mut prior: Option<Prior> = None;
    let mut follow_ups: Vec<Finding> = Vec::new();
    // The builder's last result in this run: its session is resumed for a fix round, and its
    // running totals come off the next step's. Not kept across runs: an earlier run's session
    // comes from the event log instead (`carried`).
    let mut builder: Option<StepUsage> = None;
    // How often each kind of step hit its turn limit in this run: a second hit escalates.
    let (mut builder_limits, mut review_limits) = (0u32, 0u32);
    progress(&format!("working in {}", env.path.display()));
    for round in 1..=MAX_ROUNDS {
        run.round = round;
        stop_point()?;
        let steps = &mut run.steps;
        steps.clear();
        progress(&format!(
            "round {round} of {MAX_ROUNDS}: Claude is building the change (often 5 to 30 minutes)"
        ));
        let role = if round == 1 { "build" } else { "fix" };
        // An earlier run's session is offered to the first build only.
        let earlier = carried.take();
        let built = build(
            home,
            &env,
            item,
            base,
            &feedback,
            &models,
            role,
            round,
            builder.as_ref(),
            earlier.as_ref(),
            steps,
            &commands.builder,
        );
        let (step, raw, asked) = match built {
            Ok(built) => built,
            Err(err) => {
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
                record_round(home, item, round, "turn_limit", &note, steps)?;
                if builder_limits > 1 {
                    return turn_limit_twice(home, repo, &env, item, &note, round);
                }
                progress(&format!("{note}; asking it to finish smaller"));
                feedback = format!(
                    "Your last session stopped because {note}. Finish smaller: do only what the \
                     issue needs, avoid long explorations, commit what works, and keep outputs \
                     short (pipe long ones through `tail` or `grep`)."
                );
                if let Some(p) = &prior {
                    feedback.push_str(&format!(
                        "\n\nThe review's blockers still need fixing too:\n\n{}",
                        p.blockers
                    ));
                }
                continue;
            }
        };
        builder = Some(raw);
        steps.push(step);
        // A question goes to the person at once: no checks run and no rounds are spent on it.
        if let Some(asked) = asked {
            progress("the builder asked for a decision");
            record_round(home, item, round, "question", &asked, steps)?;
            let question =
                format!("Paja's builder needs a decision before it can go on:\n\n{asked}");
            return escalated(home, repo, &env, item, &question, round);
        }
        commit_leftovers(&env, item)?;
        if git(
            &env.path,
            &["rev-list", "--count", &format!("{base}..HEAD")],
        )? == "0"
        {
            feedback = "The previous round made no commits. Make the change and commit it.".into();
            progress("the builder committed nothing; asking again");
            record_round(home, item, round, "no_change", &feedback, steps)?;
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
                record_round(home, item, round, "no_change", &feedback, steps)?;
                continue;
            }
        }
        let checked = git(&env.path, &["rev-parse", "HEAD"])?;
        // Files marked assume-unchanged or skip-worktree hide from `git status`. Sparse
        // checkout marks every file outside its cone skip-worktree, so the checks are judged
        // by whether they changed the flags. Any flag the builder's code set goes first: a
        // fresh index keeps only the ones sparse checkout gives.
        let mut flags = git(&env.path, &["ls-files", "-v"])?;
        if has_index_flags(&flags) {
            put_back(&env, &checked)?;
            flags = git(&env.path, &["ls-files", "-v"])?;
        }
        stop_point()?;
        let guard = GitGuard::new(&env.path)?;
        progress(&format!(
            "running Vasara's checks ({} command{})",
            checks.len(),
            if checks.len() == 1 { "" } else { "s" }
        ));
        let result = run_checks(&env.path, &checks);
        guard.check("Vasara's checks")?;
        // The checks run the branch's code, which could change the tree or even commit. Put it
        // back, and fail the round, so nothing lands that the checks didn't judge.
        let mut failed = result;
        let moved = git(&env.path, &["rev-parse", "HEAD"])? != checked
            || git(&env.path, &["ls-files", "-v"])? != flags
            || !git(&env.path, &["status", "--porcelain"])?.is_empty();
        if moved {
            put_back(&env, &checked)?;
            if let Ok(failure) = &mut failed {
                failure.get_or_insert_with(|| {
                    "Running the checks changed the worktree or its commits; the change was put \
                 back. Tests and builds must not write to the source tree or commit."
                        .into()
                });
            }
        }
        let failed = failed?;
        if let Some(failure) = failed {
            feedback =
                format!("Vasara's checks failed. Fix the cause, not the check:\n\n{failure}");
            if let Some(prior) = &prior {
                feedback.push_str(&format!(
                    "\n\nThe review's blockers still need fixing too:\n\n{}",
                    prior.blockers
                ));
            }
            progress("the checks failed; sending the failure back to the builder");
            record_round(home, item, round, "checks_failed", &failure, steps)?;
            continue;
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
        let risky = changed
            .split('\0')
            .filter(|path| !path.is_empty())
            .any(|path| is_high_risk(path, &high_risk));
        let reviewers = if risky { 2 } else { 1 };
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
        while i < reviewers {
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
            let reviewed = review(
                home,
                &env,
                item,
                base,
                &checked,
                prior.as_ref().filter(|_| !risky),
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
                    None => return Err(err),
                },
            }
        }
        if let Some(note) = limited {
            review_limits += 1;
            record_round(home, item, round, "turn_limit", &note, steps)?;
            if review_limits > 1 {
                return turn_limit_twice(home, repo, &env, item, &note, round);
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
            continue;
        }
        // Only the round that lands files its follow-ups: an earlier round's may be about code
        // the fix has since changed.
        follow_ups.clear();
        for review in &reviews {
            for finding in &review.follow_ups {
                if !finding.text.trim().is_empty() && !follow_ups.contains(finding) {
                    follow_ups.push(finding.clone());
                }
            }
        }
        let verdicts: Vec<&str> = reviews.iter().map(|r| r.verdict.as_str()).collect();
        progress(&format!("review verdict: {}", verdicts.join(", ")));
        record_round(
            home,
            item,
            round,
            &verdicts.join(","),
            &summaries(&reviews),
            steps,
        )?;

        if let Some(esc) = reviews.iter().find(|r| r.verdict == "escalate") {
            return escalated(home, repo, &env, item, &esc.question, round);
        }
        if verdicts.iter().all(|v| *v == "land") {
            let report = land(home, repo, &env, item, round, &checked, ci, &target);
            match &report {
                Ok(RunReport::Landed { pr, .. }) => {
                    file_follow_ups(home, repo, item, pr, &follow_ups, &labels)
                }
                // Not landed after all: keep the findings in the log, for later.
                _ => {
                    for finding in &follow_ups {
                        record_follow_up(
                            home,
                            item,
                            &finding.shown(),
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
            let question = format!(
                "The two reviewers disagree on this high-risk change.\n\n{}",
                summaries(&reviews)
            );
            return escalated(home, repo, &env, item, &question, round);
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
        // Another round runs anyway, so a nit that takes a line or two is fixed now rather
        // than filed; a nit not fixed is simply dropped.
        let nits: Vec<&str> = follow_ups
            .iter()
            .filter(|f| f.kind == FindingKind::Nit)
            .map(|f| f.text.as_str())
            .collect();
        if !nits.is_empty() {
            feedback.push_str(&format!(
                "\n\nThe reviewers also noted these nits. Fix one only if it takes a line or \
                 two; otherwise leave it:\n\n{}",
                nits.join("\n\n")
            ));
        }
        // Without named blockers there is nothing to narrow the next review to.
        prior = (!blockers.is_empty()).then(|| Prior {
            reviewed: checked,
            blockers,
            follow_ups: follow_ups.clone(),
        });
    }
    let question = format!(
        "Paja tried {MAX_ROUNDS} rounds and the change still isn't ready. Last feedback:\n\n{feedback}\n\n\
         Answer with what should change, or what is out of scope."
    );
    escalated(home, repo, &env, item, &question, MAX_ROUNDS)
}

/// The same kind of step hit its turn limit a second time in this run: ask a person rather
/// than spend another round.
fn turn_limit_twice(
    home: &Home,
    repo: &Path,
    env: &Env,
    item: &WorkItem,
    note: &str,
    round: u32,
) -> Result<RunReport> {
    let question = format!(
        "In this run {note}, the second time that step stopped at its turn limit. The issue may \
         be too big for one change: split it into smaller issues, or raise the limit with a \
         `max-turns` line in `{MODELS_FILE}` on the default branch, then answer with what you changed."
    );
    escalated(home, repo, env, item, &question, round)
}

/// A fresh environment for the item, branched from the base. A branch kept from an earlier
/// run of the same item is resumed, so its work isn't thrown away.
/// The flag says whether the branch (and so the work on it) was kept from an earlier run, as
/// opposed to made again from the base.
fn environment(home: &Home, repo: &Path, base: &str, item: &WorkItem) -> Result<(Env, bool)> {
    let name = item.env_name();
    if let Ok(env) = ahjo::get(home, &name) {
        return Ok((env, true));
    }
    let branch = format!("refs/heads/paja/{name}");
    let resume = git(repo, &["rev-parse", "--verify", "--quiet", &branch]).is_ok();
    let env = ahjo::create(
        home,
        CreateOptions {
            name: &name,
            repo,
            base: if resume { None } else { Some(base) },
        },
    )?;
    Ok((env, resume))
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
fn builder_prompt(item: &WorkItem, programs: &[String]) -> String {
    format!(
        "You are Paja's builder. Implement this GitHub issue in the current checkout, following \
         CLAUDE.md. Keep the change small and focused on the issue. Run the checks CLAUDE.md lists, \
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
) -> Result<Built> {
    let mut prompt = builder_prompt(item, programs);
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
                    // A fresh session would fail the same way: stop and say what to do.
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
    let out = note_discarded(
        home,
        item,
        &id,
        claude(home, env, base, &prompt, &tools, None, false, &args),
    )?;
    let raw = StepUsage::from_result("build", None, models, role, &out);
    Ok((raw.clone(), raw, builder_question(&out)))
}

/// What a reviewer is asked. `checked` is the commit Vasara's checks passed at, which is the
/// commit the reviewer judges.
fn review_prompt(
    item: &WorkItem,
    base: &str,
    checked: &str,
    prior: Option<&Prior>,
    diff: &str,
    checks: &[String],
    programs: &[String],
) -> String {
    // A first review reads the whole change; a later one judges only the fix for the blockers
    // an earlier review named, so review rounds converge instead of starting over.
    let scope = match prior {
        None => format!(
            "The change is the diff above (`git diff {base}...HEAD`). Read it cold against the \
             issue."
        ),
        Some(prior) => format!(
            "An earlier review of this change (at {reviewed}) found these blockers:\n\n{blockers}\n\n\
             Judge only the fix, the diff above (`git diff {reviewed}..HEAD`). Land it if it resolves those \
             blockers and adds none. That review read the whole change and named every blocker it \
             found, so don't reopen the rest. It also noted these \
             follow-ups; repeat in your `follow_ups` the ones that still apply:\n\n{follow_ups}\n",
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
         Verdicts: `land` if it does what the issue asks and has no blocker; `changes` only for \
         blockers, naming every blocker in the whole diff in `comments` (a later review reads \
         only the fix); `escalate` with a question only when the issue's \
         intent is unclear or the change would weaken a check (a person must decide). A blocker \
         is a bug in what the issue asks for, a security hole the change opens, a broken \
         CLAUDE.md rule, or missing tests for the new behaviour. Anything else goes in \
         `follow_ups`, one finding each, starting with a short title line, at most five, each \
         marked with its `kind`: `bug` for a wrong result outside what the issue asks, `risk` \
         for a security or data-loss risk or a missing test for behaviour that could break, \
         `nit` for style, naming, comment wording or a small refactor. Leave out ideas that are \
         neither. Paja files the bugs and risks together as one public issue after landing \
         (so never copy secrets, environment details or run output into them) and never files \
         nits. They never hold the change up.\n\n\
         Vasara's checks (every command in `.paja/checks` on the default branch: {checks}) already passed at \
         {checked}, the commit you are judging, so don't run them again. To probe a behaviour, \
         run one named test or the project itself, with Bash limited to {programs} and long \
         output piped through `tail -n 80` or `grep`. Read-only git is fine; you can't change \
         files.\n",
        checks = checks
            .iter()
            .map(|c| format!("`{c}`"))
            .collect::<Vec<_>>()
            .join(", "),
        programs = programs_prose(programs),
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
    let flags = ["diff", "--no-ext-diff", "--no-textconv"];
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
    let mut shown = String::new();
    let mut skipped = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for line in full.lines() {
        if line.starts_with("diff --git ") || files.is_empty() {
            files.push(String::new());
        }
        let last = files.last_mut().expect("a file was just pushed");
        last.push_str(line);
        last.push('\n');
    }
    let mut full_so_far = false;
    for file in files {
        let name = file
            .lines()
            .next()
            .and_then(|l| l.rsplit_once(" b/"))
            .map_or("(unknown file)", |(_, n)| n)
            .to_string();
        if !full_so_far && shown.chars().count() + file.chars().count() <= cap {
            shown.push_str(&file);
        } else {
            // Once one file doesn't fit, the later ones are left out too, so the diff shown
            // is a prefix of the whole.
            full_so_far = true;
            skipped.push(name);
        }
    }
    text.push_str(&format!(
        "The diff is over {cap} characters, so only its first files are here:\n\n{shown}\n\
         Not shown, because of the size cap; you must read these files yourself (`git diff \
         {range} -- <file>`): {}\n",
        skipped.join(", ")
    ));
    Ok(text)
}

#[allow(clippy::too_many_arguments)]
fn review(
    home: &Home,
    env: &Env,
    item: &WorkItem,
    base: &str,
    checked: &str,
    prior: Option<&Prior>,
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
    let mut prompt = review_prompt(item, base, &before, prior, &diff, checks, programs);
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
                        "kind": {"type": "string", "enum": ["bug", "risk", "nit"]},
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

/// Run each check in the environment. Returns the first failure's command and output. The
/// checks run the branch's code, so they get no more credentials than the agent that wrote it.
fn run_checks(dir: &Path, checks: &[String]) -> Result<Option<String>> {
    // Read before any check runs: later, the branch's code may have changed the configuration.
    let identity = git_identity(dir);
    let scratch = tempfile_dir(dir)?;
    let sbx = crate::sandbox::active();
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
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn record(home: &Home, kind: &str, data: Value) -> Result<()> {
    let lock = home.lock()?;
    home.events().append(&lock, kind, data)?;
    Ok(())
}

fn record_round(
    home: &Home,
    item: &WorkItem,
    round: u32,
    verdict: &str,
    detail: &str,
    steps: &mut Vec<StepUsage>,
) -> Result<()> {
    // Recorded once: an error later in the run must not count these steps again.
    let steps = std::mem::take(steps);
    record(
        home,
        EV_ROUND,
        json!({
            "repo": item.repo,
            "number": item.number,
            "round": round,
            "verdict": verdict,
            "detail": clip(detail),
            "steps": steps,
        }),
    )
}

fn summaries(reviews: &[Review]) -> String {
    reviews
        .iter()
        .enumerate()
        .map(|(i, r)| format!("Reviewer {}: {} — {}", i + 1, r.verdict, r.summary))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read a file as it is on the base commit, or None if it isn't there. Any other git failure
/// is an error: a check file that can't be read must never count as an empty one.
fn read_from_base(repo: &Path, base: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{base}:{path}");
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(["show", &spec])
        .output()
        .context("running git (is it installed?)")?;
    if output.status.success() {
        return Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()));
    }
    let err = String::from_utf8_lossy(&output.stderr);
    if err.contains("does not exist in") || err.contains("exists on disk, but not in") {
        return Ok(None);
    }
    bail!(
        "git show {spec} failed in {}: {}",
        repo.display(),
        err.trim()
    )
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
        for file in ["agent.rs", "land.rs", "mod.rs"] {
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
        for (text, says) in [
            ("reviewer opus", "line 1: \"reviewer opus\": unknown role"),
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
            ("max-turns builder 3", "unknown role \"builder\""),
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
        };
        let checks: Vec<String> = ["cargo fmt --all --check", "cargo test"]
            .map(String::from)
            .to_vec();
        let cargo = vec!["cargo".to_string()];
        let review_prompt = |item: &_, prior: Option<&super::Prior>| {
            super::review_prompt(item, "base1", "abc123", prior, "DIFF-TEXT", &checks, &cargo)
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
        assert!(super::builder_prompt(&item, &cargo).contains("only for local git and `cargo`"));

        // Another project: neither prompt mentions cargo or Paja as the project.
        let npm = vec!["npm".to_string(), "node".to_string()];
        let js = super::review_prompt(
            &item,
            "base1",
            "abc123",
            None,
            "DIFF-TEXT",
            &["npm test".to_string()],
            &npm,
        );
        let build = super::builder_prompt(&item, &npm);
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
    }
}
