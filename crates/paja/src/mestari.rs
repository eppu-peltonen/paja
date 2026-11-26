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

/// Most build rounds before a run escalates instead of trying again.
pub const MAX_ROUNDS: u32 = 3;
/// Vasara's checks: one shell command per line, read from the base commit.
pub const CHECKS_FILE: &str = ".paja/checks";
/// Path prefixes that make a change high risk (two reviewers), read from the base commit.
pub const HIGH_RISK_FILE: &str = ".paja/high-risk";
/// The model and effort for each agent role, read from the base commit.
pub const MODELS_FILE: &str = ".paja/models";
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
    /// Findings that don't block landing. Mestari files each as a `paja-follow-up` issue once
    /// the change lands, for a person to queue, instead of holding this change up.
    #[serde(default)]
    pub follow_ups: Vec<String>,
}

/// What one agent step spent, kept from the `claude -p` result. A field the result doesn't
/// have (an older Claude Code, a fake in tests) stays `None`: unknown, never zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepUsage {
    /// `build` or `review`.
    pub step: String,
    /// Which reviewer of the round (1 or 2), for a review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<u32>,
    /// The role's line in `.paja/models` (`build`, `fix`, `review` and so on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The model `.paja/models` asked for, or None for the account's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What each model the session used spent, by model name (Claude Code's `modelUsage`).
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, ModelUsage>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    /// Claude Code's estimate of the step's cost in US dollars.
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub session_id: Option<String>,
    /// The step resumed an earlier session of the same run (a builder's fix round).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resumed: bool,
    /// The `--fallback-model` the step was given, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// The fallback model served part of the step: it spent something in `models`, so the role
    /// didn't run only on the model `.paja/models` chose for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fell_back: bool,
}

/// Whether `fallback` spent anything in a step's `models`. Claude Code names models by their
/// full id, so an alias such as `sonnet` matches any id that contains it.
fn served_by(models: &std::collections::BTreeMap<String, ModelUsage>, fallback: &str) -> bool {
    models.iter().any(|(name, used)| {
        name.contains(fallback)
            && (used.output_tokens.is_some_and(|n| n > 0) || used.cost_usd.is_some_and(|c| c > 0.0))
    })
}

/// One model's share of a step, from Claude Code's `modelUsage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl StepUsage {
    /// This step's own share of a resumed session. Claude Code reports `total_cost_usd` and
    /// `modelUsage` for the whole session so far, so the earlier result's values come off;
    /// `usage` already counts only this run.
    pub fn since(&self, earlier: &StepUsage) -> Self {
        fn less<T: std::ops::Sub<Output = T> + PartialOrd + Copy>(
            now: Option<T>,
            before: Option<T>,
        ) -> Option<T> {
            match (now, before) {
                (Some(now), Some(before)) if now >= before => Some(now - before),
                // Unknown, or a total that went down: say unknown, never guess.
                (Some(_), Some(_)) | (Some(_), None) | (None, _) => None,
            }
        }
        let mut step = self.clone();
        step.cost_usd = less(self.cost_usd, earlier.cost_usd);
        for (name, used) in &mut step.models {
            let before = earlier.models.get(name).cloned().unwrap_or(ModelUsage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cache_read_input_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
                cost_usd: Some(0.0),
            });
            used.input_tokens = less(used.input_tokens, before.input_tokens);
            used.output_tokens = less(used.output_tokens, before.output_tokens);
            used.cache_read_input_tokens =
                less(used.cache_read_input_tokens, before.cache_read_input_tokens);
            used.cache_creation_input_tokens = less(
                used.cache_creation_input_tokens,
                before.cache_creation_input_tokens,
            );
            used.cost_usd = less(used.cost_usd, before.cost_usd);
        }
        step.resumed = true;
        // Judged on this step's own share: an earlier round's fallback doesn't carry over.
        step.fell_back = step
            .fallback
            .as_deref()
            .is_some_and(|f| served_by(&step.models, f));
        step
    }

    /// Read the usage fields from a `claude -p` JSON result.
    pub fn from_result(
        step: &str,
        reviewer: Option<u32>,
        chosen: &Models,
        role: &str,
        result: &Value,
    ) -> Self {
        let usage = &result["usage"];
        let models = result["modelUsage"]
            .as_object()
            .map(|models| {
                models
                    .iter()
                    .map(|(name, m)| {
                        let used = ModelUsage {
                            input_tokens: m["inputTokens"].as_u64(),
                            output_tokens: m["outputTokens"].as_u64(),
                            cache_read_input_tokens: m["cacheReadInputTokens"].as_u64(),
                            cache_creation_input_tokens: m["cacheCreationInputTokens"].as_u64(),
                            cost_usd: m["costUSD"].as_f64(),
                        };
                        (name.clone(), used)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fallback = chosen.fallback_for(role).map(str::to_string);
        StepUsage {
            step: step.to_string(),
            reviewer,
            role: Some(role.to_string()),
            model: chosen.roles.get(role).map(|m| m.model.clone()),
            input_tokens: usage["input_tokens"].as_u64(),
            output_tokens: usage["output_tokens"].as_u64(),
            cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64(),
            cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
            cost_usd: result["total_cost_usd"].as_f64(),
            num_turns: result["num_turns"].as_u64(),
            duration_ms: result["duration_ms"].as_u64(),
            session_id: result["session_id"].as_str().map(str::to_string),
            resumed: false,
            fell_back: fallback.as_deref().is_some_and(|f| served_by(&models, f)),
            fallback,
            models,
        }
    }
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
    pub fn parse(text: &str) -> Result<Self> {
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
            let at = || format!("{MODELS_FILE} on main, line {}: {line:?}", i + 1);
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
    follow_ups: Vec<String>,
}

pub struct RunOptions<'a> {
    /// Any path inside the repository's main checkout.
    pub repo: &'a Path,
    /// A person has checked the repository's git configuration after Paja stopped on
    /// tampering, and Paja may run git there again.
    pub tamper_checked: bool,
}

/// `claude`, or the program named by `PAJA_CLAUDE` (tests point it at a fake).
fn claude_program() -> String {
    std::env::var("PAJA_CLAUDE").unwrap_or_else(|_| "claude".to_string())
}

/// Tell the person watching what `paja run` is doing now: one line on stderr, so stdout keeps
/// its one JSON value per item. A long step says so, since silence reads as broken.
/// Text from outside (an issue title, a verdict) can carry terminal escapes or line breaks
/// that would fake or rewrite earlier lines, so control characters become spaces and a long
/// message is cut.
pub fn progress(message: &str) {
    use std::io::Write;
    // A closed stderr must never abort a run halfway, for example between merge and record.
    let _ = writeln!(std::io::stderr(), "paja: {}", shown_line(message));
}

/// Characters that can break, reorder or hide text on a terminal: controls, line and
/// paragraph separators, bidi overrides and isolates, and zero-width marks.
fn unsafe_on_a_terminal(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

fn shown_line(message: &str) -> String {
    const MAX: usize = 300;
    let mut line: String = message
        .chars()
        .map(|c| if unsafe_on_a_terminal(c) { ' ' } else { c })
        .take(MAX)
        .collect();
    if message.chars().count() > MAX {
        line.push_str("...");
    }
    line
}

/// The sandbox agents and checks run in, when `PAJA_SBX` turns it on. Refuses when it is on but
/// can't start; says so when it is off.
fn start_sandbox(home: &Home, repo: &Path) -> Result<Option<crate::sandbox::Sbx>> {
    if crate::sandbox::configured().is_none() {
        progress(&format!(
            "no sandbox ({} is not set): agents and checks run as you, behind the deny-list hook only",
            crate::sandbox::PROGRAM_VAR
        ));
        return Ok(None);
    }
    progress("starting the sandbox");
    let envs = home.envs_dir();
    std::fs::create_dir_all(&envs).with_context(|| format!("creating {}", envs.display()))?;
    // The worktrees keep their git data in the repository's own `.git`, which commits need.
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    crate::sandbox::start(&[envs, PathBuf::from(common)])
}

/// `argv` run in `sbx`, in `dir`, with the throwaway variables [`without_credentials`] gives
/// code on the host, and nothing else.
fn in_sandbox(
    sbx: &crate::sandbox::Sbx,
    dir: &Path,
    identity: Vec<(&str, String)>,
    script_or_argv: Result<&str, &[String]>,
) -> Result<Command> {
    let mut vars = Command::new("env");
    without_credentials(&mut vars, Path::new(crate::sandbox::SCRATCH), identity);
    let env = crate::sandbox::env_of(&vars);
    match script_or_argv {
        Ok(script) => sbx.script(dir, &env, script),
        Err(argv) => sbx.command(dir, &env, argv),
    }
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
    // Before the queue is touched: a sandbox that can't start is the person's to fix, and must
    // not park an item.
    crate::sandbox::activate(start_sandbox(home, &repo)?);
    git(&repo, &["fetch", "-q", "origin", "main"]).context(
        "fetching origin/main; check the network and that `origin` is the GitHub repository",
    )?;
    let base = git(&repo, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])?;
    progress("checking the queue (issues labelled paja)");
    jono::sync(home, &repo)?;
    let config = read_config(&repo, &base)?;
    // The claim's lock is held until this pass is done with the item, however it ends.
    let Some(jono::RunClaim {
        item,
        lock: _working,
        resumed,
    }) = jono::claim_for_run(home)?
    else {
        if let Some(waiting) = jono::waiting(home)? {
            progress(&format!("nothing is ready: {waiting}"));
        }
        return Ok(RunReport::Idle);
    };
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
            jono::set_status(home, item.number, Status::Escalated, Some(&question))?;
            // The steps that ran before the error were spent too; keep them in the item's usage.
            if !run.steps.is_empty() {
                let error = format!("{err:#}");
                record_round(home, &item, rounds, "error", &error, &mut run.steps)?;
            }
            record(
                home,
                EV_ESCALATED,
                json!({ "number": item.number, "rounds": rounds }),
            )?;
            if is_tampered(&err) {
                // gh would run git in the repository to find it, and removing the environment
                // runs git too; stop instead, loop included, and let the error tell the person.
                // The event keeps later runs out until a person has checked.
                record(
                    home,
                    EV_TAMPERED,
                    json!({ "repo": key, "number": item.number, "error": format!("{err:#}") }),
                )?;
                return Err(err);
            }
            let asked = comment(&repo, &item, &question);
            progress(&parked_line(
                &format!("#{} stopped on an error and was parked", item.number),
                item.number,
                asked.err(),
            ));
            let _ = ahjo::remove(home, &format!("work-{}", item.number), true);
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

/// Vasara's check commands, the high-risk paths and the model per role, as `main` has them.
struct Config {
    checks: Vec<String>,
    high_risk: Vec<String>,
    models: Models,
}

/// Read and check `main`'s configuration once, before an item is claimed: a mistake there is
/// one fix on `main`, so it stops the pass instead of parking every item in turn.
fn read_config(repo: &Path, base: &str) -> Result<Config> {
    let checks = read_from_base(repo, base, CHECKS_FILE)?.with_context(|| {
        format!("{CHECKS_FILE} is missing on main; add the check commands there")
    })?;
    let checks: Vec<String> = lines(&checks);
    if checks.is_empty() {
        // An empty gate would pass everything; that is never what main meant.
        bail!("{CHECKS_FILE} on main has no commands; add the check commands there");
    }
    let high_risk = lines(&read_from_base(repo, base, HIGH_RISK_FILE)?.unwrap_or_default());
    let models = Models::parse(&read_from_base(repo, base, MODELS_FILE)?.unwrap_or_default())?;
    Ok(Config {
        checks,
        high_risk,
        models,
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
        checks,
        high_risk,
        models,
    } = config;
    let (env, kept) = environment(home, repo, base, item)?;
    // An earlier run's builder session is resumed only for work still on its branch: an
    // environment made again from `main` starts a fresh one.
    let mut carried = if kept && (resumed || !item.answers.is_empty()) {
        carried_session(home, item, &env.branch)?
    } else {
        None
    };
    record(
        home,
        EV_STARTED,
        json!({ "number": item.number, "env": env.name, "base": base }),
    )?;

    let mut feedback = String::new();
    let mut prior: Option<Prior> = None;
    let mut follow_ups: Vec<String> = Vec::new();
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
                if !finding.trim().is_empty() && !follow_ups.contains(finding) {
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
            let report = land(home, repo, &env, item, round, &checked);
            match &report {
                Ok(RunReport::Landed { pr, .. }) => {
                    file_follow_ups(home, repo, item, pr, &follow_ups)
                }
                // Not landed after all: keep the findings in the log, for later.
                _ => {
                    for finding in &follow_ups {
                        record_follow_up(
                            home,
                            item,
                            finding,
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
         `max-turns` line in `{MODELS_FILE}` on main, then answer with what you changed."
    );
    escalated(home, repo, env, item, &question, round)
}

/// A fresh environment for the item, branched from the base. A branch kept from an earlier
/// run of the same item is resumed, so its work isn't thrown away.
/// The flag says whether the branch (and so the work on it) was kept from an earlier run, as
/// opposed to made again from the base.
fn environment(home: &Home, repo: &Path, base: &str, item: &WorkItem) -> Result<(Env, bool)> {
    let name = format!("work-{}", item.number);
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
    number: u64,
    branch: &str,
) -> Option<(String, bool)> {
    let mut last: Option<(String, bool)> = None;
    for event in events.iter().filter(|e| e.data["number"] == number) {
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

/// What the recorded `run.round` steps of `session` add up to, or None when none was recorded
/// (a session that was killed before its result) or a figure is unknown.
fn recorded_totals(
    events: &[crate::events::Event],
    number: u64,
    session: &str,
) -> Option<StepUsage> {
    fn plus<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
        Some(a? + b?)
    }
    let mut total: Option<StepUsage> = None;
    for event in events
        .iter()
        .filter(|e| e.kind == EV_ROUND && e.data["number"] == number)
    {
        let Some(steps) = event.data["steps"].as_array() else {
            continue;
        };
        for step in steps {
            let Ok(step) = serde_json::from_value::<StepUsage>(step.clone()) else {
                continue;
            };
            if step.step != "build" || step.session_id.as_deref() != Some(session) {
                continue;
            }
            total = Some(match total {
                None => step,
                Some(mut sum) => {
                    sum.cost_usd = plus(sum.cost_usd, step.cost_usd);
                    for (name, used) in step.models {
                        let slot = sum.models.entry(name).or_insert_with(|| ModelUsage {
                            input_tokens: Some(0),
                            output_tokens: Some(0),
                            cache_read_input_tokens: Some(0),
                            cache_creation_input_tokens: Some(0),
                            cost_usd: Some(0.0),
                        });
                        slot.input_tokens = plus(slot.input_tokens, used.input_tokens);
                        slot.output_tokens = plus(slot.output_tokens, used.output_tokens);
                        slot.cache_read_input_tokens =
                            plus(slot.cache_read_input_tokens, used.cache_read_input_tokens);
                        slot.cache_creation_input_tokens = plus(
                            slot.cache_creation_input_tokens,
                            used.cache_creation_input_tokens,
                        );
                        slot.cost_usd = plus(slot.cost_usd, used.cost_usd);
                    }
                    sum
                }
            });
        }
    }
    total.map(|mut sum| {
        sum.session_id = Some(session.to_string());
        sum
    })
}

/// The earlier builder session to resume in this run, with the short prompt it gets. Nothing
/// when the log holds none, or when the sandbox lost its transcript.
fn carried_session(home: &Home, item: &WorkItem, branch: &str) -> Result<Option<Carried>> {
    let events = home.events().read_all()?;
    let Some((session_id, escalated)) = earlier_session(&events, item.number, branch) else {
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
        totals: recorded_totals(&events, item.number, &session_id),
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
                json!({ "number": item.number, "session_id": session_id }),
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

/// How the builder asks the person for a decision: a line of its final message starting with
/// this, followed by the question.
const ASK: &str = "PAJA-QUESTION:";

/// The question the builder asked in its final message, if it asked one.
fn builder_question(out: &Value) -> Option<String> {
    let text = out["result"].as_str()?;
    let start = text
        .match_indices(ASK)
        .find(|(at, _)| *at == 0 || text[..*at].ends_with('\n'))?
        .0;
    let question = text[start + ASK.len()..].trim();
    (!question.is_empty()).then(|| question.to_string())
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
) -> Result<Built> {
    let mut prompt = format!(
        "You are Paja's builder. Implement this GitHub issue in the current checkout, following \
         CLAUDE.md. Keep the change small and focused on the issue. Run the checks CLAUDE.md lists, \
         and commit your work on the current branch (never push, never switch branches). Commit \
         with `git add -A && git commit -m '<message>'`, the message one single-quoted argument \
         that ends with `Closes #{number}`. Read and change files with the file tools; Bash is \
         only for cargo and local git, and Paja's deny-list hook blocks anything else (no `$(`, \
         heredocs or redirects). When output is long, pipe it through `tail -n 80` or `grep`. If the \
         issue leaves a decision only the person can make (what to build or what is out of scope, \
         not how to build it), don't put the question in code, tests or commits: commit nothing, \
         and end your final message with a line that starts with `{ASK}` followed by the \
         question, the options and your recommendation. Paja then asks the person on the \
         issue.\n\nIssue #{number}: {title}\n\n{body}\n{answers}",
        number = item.number,
        title = item.title,
        body = item.body,
        answers = answers(item)
    );
    let again = format!(
        "This is another round on the same branch (based on {base}). Address this first:\n\n{feedback}\n"
    );
    if !feedback.is_empty() {
        prompt.push_str(&format!("\n{again}"));
    }
    let tools = [
        "Read",
        "Edit",
        "Write",
        "Glob",
        "Grep",
        "Bash(cargo *)",
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
    ];
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
fn review_prompt(item: &WorkItem, base: &str, checked: &str, prior: Option<&Prior>) -> String {
    // A first review reads the whole change; a later one judges only the fix for the blockers
    // an earlier review named, so review rounds converge instead of starting over.
    let scope = match prior {
        None => format!(
            "The change is `git diff {base}...HEAD`. Read it cold against the issue below."
        ),
        Some(prior) => format!(
            "An earlier review of this change (at {reviewed}) found these blockers:\n\n{blockers}\n\n\
             Judge only the fix: `git diff {reviewed}..HEAD`. Land it if it resolves those \
             blockers and adds none. That review read the whole change and named every blocker it \
             found, so don't reopen the rest. It also noted these \
             follow-ups; repeat in your `follow_ups` the ones that still apply:\n\n{follow_ups}\n",
            reviewed = prior.reviewed,
            blockers = prior.blockers,
            follow_ups = if prior.follow_ups.is_empty() {
                "(none)".to_string()
            } else {
                prior.follow_ups.join("\n\n")
            }
        ),
    };
    format!(
        "You are an independent reviewer for Paja. You did not write this change; decide \
         whether it lands.\n\n{scope} Judge it by the rules in CLAUDE.md as they are on main \
         (`git show {base}:CLAUDE.md`), not the branch's copy.\n\n\
         Verdicts: `land` if it does what the issue asks and has no blocker; `changes` only for \
         blockers, naming every blocker in the whole diff in `comments` (a later review reads \
         only the fix); `escalate` with a question only when the issue's \
         intent is unclear or the change would weaken a check (a person must decide). A blocker \
         is a bug in what the issue asks for, a security hole the change opens, a broken \
         CLAUDE.md rule, or missing tests for the new behaviour. Anything else, including \
         hardening ideas, edge cases outside the issue and nits, goes in `follow_ups`, one \
         finding each, starting with a short title line, at most five; Paja files them as public \
         issues after landing, so never copy secrets, environment details or run output into \
         them. They never hold the change up.\n\n\
         Vasara's checks (every command in `.paja/checks` on main: formatting, lints and the full \
         test suite) already passed at {checked}, the commit you are judging, so don't run \
         `cargo fmt`, `cargo clippy` or the whole `cargo test` again. To probe a behaviour, run \
         one named test (`cargo test <name>`) or the CLI, with long output piped through \
         `tail -n 80` or `grep`. Read-only git is fine; you can't change files.\n\n\
         Issue #{number}: {title}\n\n{body}\n{answers}",
        number = item.number,
        title = item.title,
        body = item.body,
        answers = answers(item)
    )
}

/// What a person answered to Paja's earlier questions on the item, for the agents' prompts.
/// Agents can't read the issue's comments, so this is how an answer reaches the next run.
fn answers(item: &WorkItem) -> String {
    if item.answers.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "\nA person answered Paja's earlier questions about this issue. The answers settle what \
         the issue leaves open; follow them:\n",
    );
    for answer in &item.answers {
        text.push('\n');
        text.push_str(&show_answer(answer));
    }
    text
}

/// One answer with the question it settles, as the prompts show it.
fn show_answer(answer: &jono::Answer) -> String {
    let mut text = String::new();
    if let Some(question) = &answer.question {
        text.push_str(&format!("Question: {question}\n"));
    }
    text.push_str(&format!("Answer: {}\n", answer.answer));
    text
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
    let mut prompt = review_prompt(item, base, &before, prior);
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
            "follow_ups": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["verdict", "summary"]
    });
    let tools = [
        "Read",
        "Glob",
        "Grep",
        "Bash(cargo *)",
        "Bash(git status*)",
        "Bash(git diff*)",
        "Bash(git log*)",
        "Bash(tail *)",
        "Bash(head *)",
        "Bash(grep *)",
        "Bash(wc *)",
        "Bash(pwd)",
        "Bash(git show*)",
    ];
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

/// The built-in tools an agent gets: reviewers read and run commands, builders also edit.
/// `--allowedTools` and the deny-list hook still decide what each call may do.
fn agent_tools(read_only: bool) -> &'static str {
    if read_only {
        "Read,Glob,Grep,Bash"
    } else {
        "Read,Edit,Write,Glob,Grep,Bash"
    }
}

/// A session stopped at its `--max-turns` limit. Carries Claude Code's result, which still
/// reports what the session spent and its session id.
#[derive(Debug)]
struct TurnLimit(Value);

impl std::fmt::Display for TurnLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Claude Code stopped at its turn limit")
    }
}

impl std::error::Error for TurnLimit {}

fn turn_limit(err: &anyhow::Error) -> Option<&TurnLimit> {
    err.chain().find_map(|e| e.downcast_ref::<TurnLimit>())
}

/// The deny-list hook never ran during a session, so its work was not trusted and thrown away.
/// Its session is never resumed.
#[derive(Debug)]
struct HookNeverRan(String);

impl std::fmt::Display for HookNeverRan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HookNeverRan {}

/// Claude Code ran and exited with an error or reported one: nothing Paja itself refused, so a
/// failed resume may be tried again as a fresh session. Holds the JSON result when Claude Code
/// printed one.
#[derive(Debug)]
struct ClaudeFailed(String, Option<Value>);

impl std::fmt::Display for ClaudeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClaudeFailed {}

/// Why a failed `claude -p` result must not be retried as a fresh session, and what the person
/// does about it: a usage or rate limit, or a sign-in that no longer works, fails a new session
/// the same way.
fn not_retried(out: &Value) -> Option<&'static str> {
    match out["api_error_status"].as_u64() {
        Some(429) => Some(
            "Claude Code hit a usage or rate limit; wait until it resets, then answer the item to \
             run it again",
        ),
        Some(401 | 403) => Some(
            "Claude Code's sign-in was refused; run `claude` and sign in again (or check \
             ANTHROPIC_API_KEY), then answer the item to run it again",
        ),
        _ => None,
    }
}

/// Run headless Claude Code in the environment and return its JSON result.
#[allow(clippy::too_many_arguments)]
fn claude(
    home: &Home,
    env: &Env,
    base: &str,
    prompt: &str,
    tools: &[&str],
    schema: Option<&Value>,
    read_only: bool,
    model_args: &[String],
) -> Result<Value> {
    let program = claude_program();
    let harness = match read_from_base(&env.path, base, "CLAUDE.md")? {
        Some(text) => format!("The project's CLAUDE.md, as it is on main:\n\n{text}"),
        None => String::new(),
    };
    let git_identity = git_identity(&env.path);
    let guard = GitGuard::new(&env.path)?;
    // Agents get a throwaway PAJA_HOME, so trying the CLI never touches the factory's state.
    let scratch = tempfile_dir(&env.path)?;
    // In the sandbox the deny-list hook (a host program) doesn't run; the sandbox is the guard.
    let sbx = crate::sandbox::active();
    let (settings, stamp, exe) = if sbx.is_some() {
        (String::new(), PathBuf::new(), PathBuf::new())
    } else {
        match hook_settings(&env.path, home, read_only) {
            Ok(made) => made,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&scratch);
                return Err(err);
            }
        }
    };
    let mut args: Vec<String> = ["-p", prompt, "--output-format", "json", "--permission-mode"]
        .into_iter()
        .map(String::from)
        // The sandbox image defaults to bypassPermissions, which is never acceptable.
        .chain([if sbx.is_some() { "auto" } else { "acceptEdits" }.to_string()])
        // No settings files at all, only the deny-list hook: neither the owner's settings nor
        // a settings file on the branch can loosen or switch it off.
        .chain(["--setting-sources".to_string(), String::new()])
        .collect();
    if sbx.is_none() {
        args.extend(["--settings".to_string(), settings]);
    }
    // With no setting sources Claude Code loads no CLAUDE.md, so pass the harness from
    // main: the branch under review can't rewrite the rules its agents follow.
    args.extend(["--append-system-prompt".to_string(), harness]);
    // No MCP servers from anyone's configuration: agents get only Claude Code's own tools.
    args.push("--strict-mcp-config".to_string());
    // Only the built-in tools the role uses are defined at all, so no other tool (subagents,
    // web tools) reaches the agent or spends prompt tokens on its definition.
    args.extend(["--tools".to_string(), agent_tools(read_only).to_string()]);
    // Per-machine details (the worktree's path, its git status) go in the first message
    // instead of the system prompt, so sessions in different environments share a cached
    // prompt prefix.
    args.push("--exclude-dynamic-system-prompt-sections".to_string());
    // The model and effort come from `.paja/models` on main, so a change can't pick the models
    // that judge it.
    args.extend(model_args.iter().cloned());
    args.push("--allowedTools".to_string());
    args.extend(tools.iter().map(|t| t.to_string()));
    if let Some(schema) = schema {
        args.extend(["--json-schema".to_string(), schema.to_string()]);
    }
    let mut cmd = match &sbx {
        Some(sbx) => {
            let mut argv = vec![program.clone()];
            argv.extend(args);
            in_sandbox(sbx, &env.path, git_identity, Err(&argv))?
        }
        None => {
            let mut cmd = Command::new(&program);
            cmd.current_dir(&env.path);
            without_credentials(&mut cmd, &scratch, git_identity);
            cmd.args(&args);
            cmd
        }
    };
    let before = git_head(&env.path);
    let status_before = git_status(&env.path);
    let output = crate::platform::output_until_stopped(cmd.stdin(std::process::Stdio::null()));
    let _ = std::fs::remove_dir_all(&scratch);
    let hooked = stamp.exists();
    let _ = std::fs::remove_file(&stamp);
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    guard.check("the agent")?;
    // Code the agent ran could also have swapped the hook's copy during the session.
    let hooked = sbx.is_some() || hooked && hook_copy_intact(&exe);
    if let (Some(sbx), Ok(out)) = (&sbx, &output) {
        // Not a failed check or session: the person has to sign in, once.
        if crate::sandbox::signed_out(
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ) {
            bail!("{}", sbx.sign_in_hint());
        }
    }
    // A session that took more than one turn, or changed the worktree, used tools, so the hook
    // must have run. If it didn't, something (such as an administrator's policy) switched it
    // off and nothing the agent did can be trusted: throw it away, so a later run can't resume
    // and land it.
    let turns = output
        .as_ref()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|value| value["num_turns"].as_u64())
        .unwrap_or(0);
    if !hooked
        && output.is_ok()
        && (turns > 1 || changed_since(&env.path, before.as_deref(), status_before.as_deref()))
    {
        let fate = match before.as_deref() {
            Some(before) => match discard(env, before) {
                Ok(()) => "was discarded".to_string(),
                Err(err) => format!(
                    "could not be discarded ({err:#}); delete the branch {} before running it again",
                    env.branch
                ),
            },
            None => format!(
                "is still on the branch {} (its starting commit was unknown); check or delete it \
                 before running it again",
                env.branch
            ),
        };
        return Err(HookNeverRan(format!(
            "Claude Code never called Paja's deny-list hook during the agent's run in {}, or the \
             hook's copy {} changed during it, so its work is not trusted and {fate}; check for \
             settings that disable hooks (such as `disableAllHooks` in managed settings) and run \
             again",
            env.path.display(),
            exe.display()
        ))
        .into());
    }
    let output = output.with_context(|| {
        format!(
            "running `{program}` (install Claude Code and sign in{})",
            crate::platform::CLAUDE_HINT
        )
    })?;
    // Checked before the exit status: a session at its turn limit exits with an error.
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
        if value["subtype"] == "error_max_turns" {
            return Err(TurnLimit(value).into());
        }
    }
    if !output.status.success() {
        return Err(ClaudeFailed(
            format!(
                "`{program} -p` failed in {}: {}",
                env.path.display(),
                tail(&output)
            ),
            serde_json::from_slice(&output.stdout).ok(),
        )
        .into());
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("`{program} -p` printed something that is not JSON"))?;
    if value.get("is_error") == Some(&Value::Bool(true)) {
        return Err(ClaudeFailed(
            format!("`{program} -p` reported an error: {}", value["result"]),
            Some(value),
        )
        .into());
    }
    Ok(value)
}

/// The owner's commit name and email, and their global ignore file, which agents' git uses
/// while git's global configuration is hidden from them.
fn git_identity(repo: &Path) -> Vec<(&'static str, String)> {
    let get = |key: &str| {
        let out = git_command()
            .arg("-C")
            .arg(repo)
            .args(["config"])
            .args((key == "core.excludesFile").then_some("--path"))
            .arg(key)
            .output()
            .ok()?;
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !value.is_empty()).then_some(value)
    };
    let mut vars = Vec::new();
    if let Some(name) = get("user.name") {
        vars.push(("GIT_AUTHOR_NAME", name.clone()));
        vars.push(("GIT_COMMITTER_NAME", name));
    }
    if let Some(email) = get("user.email") {
        vars.push(("GIT_AUTHOR_EMAIL", email.clone()));
        vars.push(("GIT_COMMITTER_EMAIL", email));
    }
    // Without it, editor and system files the owner ignores everywhere would be committed.
    // Mestari sets two config entries; when there is no ignore file, the second repeats the
    // first, so git's own default ignore file still applies.
    let (key, value) = match get("core.excludesFile") {
        Some(file) => ("core.excludesFile", file),
        None => ("credential.helper", String::new()),
    };
    vars.push(("GIT_CONFIG_KEY_1", key.into()));
    vars.push(("GIT_CONFIG_VALUE_1", value));
    vars
}

fn git_head(repo: &Path) -> Option<String> {
    let out = git_command()
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether the worktree's HEAD or its uncommitted changes differ from before the run.
fn changed_since(repo: &Path, head: Option<&str>, status: Option<&str>) -> bool {
    git_head(repo).as_deref() != head || git_status(repo).as_deref() != status
}

/// `git status --porcelain`, to compare the worktree before and after an agent's run.
fn git_status(repo: &Path) -> Option<String> {
    let out = git_command()
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a `git ls-files -v` listing marks any file assume-unchanged (a lowercase tag) or
/// skip-worktree (`S`).
fn has_index_flags(listing: &str) -> bool {
    listing
        .lines()
        .any(|line| line.starts_with(|c: char| c.is_ascii_lowercase() || c == 'S'))
}

/// Put the environment's branch, index and files back at `to` after code ran there. The
/// index is rebuilt from scratch, so no file stays hidden behind an assume-unchanged flag,
/// and only sparse checkout's own skip-worktree flags come back. Ignored files such as
/// `target/` stay.
fn put_back(env: &Env, to: &str) -> Result<()> {
    let index = git(&env.path, &["rev-parse", "--git-path", "index"])?;
    let index = env.path.join(index);
    match std::fs::remove_file(&index) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("removing git's index {}", index.display()))
        }
    }
    git(
        &env.path,
        &["checkout", "-q", "-f", "-B", env.branch.as_str(), to],
    )?;
    git(&env.path, &["clean", "-q", "-f", "-d"])?;
    Ok(())
}

/// Put the environment's own branch back at `to` and drop every file made since, ignored
/// ones included, even if the agent switched branches.
fn discard(env: &Env, to: &str) -> Result<()> {
    for args in [
        &["checkout", "-q", "-f", "-B", &env.branch, to][..],
        &["clean", "-q", "-f", "-d", "-x"],
    ] {
        let status = git_command()
            .arg("-C")
            .arg(&env.path)
            .args(args)
            .status()
            .context("running git")?;
        if !status.success() {
            bail!("`git {}` failed in {}", args.join(" "), env.path.display());
        }
    }
    Ok(())
}

/// Keeps stamp directory names unique within one process.
static STAMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Claude Code settings that run every tool call through `paja hook`, bounded to `root`.
/// Before handing them out, check that the hook really blocks a push and allows a build; an
/// agent never starts under a hook that doesn't.
fn hook_settings(root: &Path, home: &Home, read_only: bool) -> Result<(String, PathBuf, PathBuf)> {
    refuse_hook_policies()?;
    let exe = hook_binary(home)?;
    // The stamp sits in a fresh directory only this user can enter, so nobody can plant it
    // beforehand; creating the directory fails if the name is already taken.
    let private = std::env::temp_dir().join(format!(
        "paja-stamp-{}-{}-{}",
        std::process::id(),
        crate::events::now_ms(),
        STAMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    crate::platform::create_private_dir(&private)
        .with_context(|| format!("creating the hook's stamp directory {}", private.display()))?;
    let stamp = private.join("stamp");
    // Test the very command Claude Code will run, with a throwaway stamp of its own, so a
    // path the shell reads differently fails here and not on every tool call.
    let trial = private.join("self-test");
    let tested = hook_command(&exe, root, home.root(), &trial, read_only)
        .and_then(|command| self_test_hook(&command, root, read_only));
    let _ = std::fs::remove_file(&trial);
    if let Err(err) = tested {
        let _ = std::fs::remove_dir(&private);
        return Err(err);
    }
    let command = hook_command(&exe, root, home.root(), &stamp, read_only)?;
    // Claude Code's timeout also counts as "allow", so leave the hook's own deadline room.
    let timeout = crate::hook::HOOK_DEADLINE_SECS * 3;
    let settings = json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": command, "timeout": timeout}]
            }]
        }
    })
    .to_string();
    Ok((settings, stamp, exe))
}

/// The shell command Claude Code runs for every tool call. It only blocks on exit 2, so a
/// hook that can't start (127), is killed or fails in any other way must block too.
fn hook_command(
    exe: &Path,
    root: &Path,
    home: &Path,
    stamp: &Path,
    read_only: bool,
) -> Result<String> {
    Ok(format!(
        "{} hook --root {} --home {} --stamp {}{} || exit 2",
        crate::platform::quote_hook_word(exe)?,
        crate::platform::quote_hook_word(root)?,
        crate::platform::quote_hook_word(home)?,
        crate::platform::quote_hook_word(stamp)?,
        if read_only { " --read-only" } else { "" }
    ))
}

/// Run the hook `command` through the shell, as Claude Code does, and check that it blocks a
/// push (a commit when `read_only`) and allows a build.
fn self_test_hook(command: &str, root: &Path, read_only: bool) -> Result<()> {
    let (blocked, build) = crate::hook::self_test_inputs(root, read_only);
    let (code, stderr) = run_hook_once(command, &blocked)?;
    let blocks = code == Some(2) && stderr.contains("Blocked by Paja's deny-list");
    let (code, _) = run_hook_once(command, &build)?;
    if !blocks || code != Some(0) {
        bail!(
            "the deny-list hook command `{command}` failed its self-test (it must block `{}` \
             and allow `cargo test`), so no agent will start; rebuild paja from main, and \
             check that every path in the command exists as written",
            if read_only { "git commit" } else { "git push" }
        );
    }
    Ok(())
}

/// Managed (administrator) settings load whatever `--setting-sources` says, and two of their
/// keys would silently drop Mestari's hook. Refuse to start agents on such a machine.
fn refuse_hook_policies() -> Result<()> {
    let mut paths = Vec::new();
    for dir in crate::platform::managed_settings_dirs() {
        let dir = dir.as_path();
        paths.push(dir.join("managed-settings.json"));
        // Drop-in files next to it carry the same keys.
        if let Ok(entries) = std::fs::read_dir(dir.join("managed-settings.d")) {
            let mut drop_ins: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect();
            drop_ins.sort();
            paths.extend(drop_ins);
        }
    }
    for path in &paths {
        let path = path.display();
        let text = match std::fs::read_to_string(path.to_string()) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "reading Claude Code's managed settings {path}, which could switch off \
                         Paja's deny-list hook; make it readable or run Paja elsewhere"
                    )
                })
            }
        };
        let settings: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "reading Claude Code's managed settings {path}, which is not valid JSON; ask \
                     the machine's administrator to fix it, or run Paja on another machine"
            )
        })?;
        for key in ["disableAllHooks", "allowManagedHooksOnly"] {
            if settings[key] == Value::Bool(true) {
                bail!(
                    "Claude Code's managed settings ({path}) set `{key}`, which would switch off \
                     Paja's deny-list hook, so no agent will start; ask the machine's \
                     administrator to drop it, or run Paja on another machine"
                );
            }
        }
    }
    Ok(())
}

/// This paja's own bytes, read once per process: what the hook copy must match, whatever later
/// happens to the file.
static RUNNING: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// Whether the hook's copy still matches this paja byte for byte.
fn hook_copy_intact(exe: &Path) -> bool {
    RUNNING
        .get()
        .is_some_and(|bytes| read_regular(exe).as_deref() == Some(bytes.as_slice()))
}

/// A git configuration file's entries as git itself reads them, leaving out `branch.*`:
/// ordinary git work (`push -u`, `checkout -b`) writes those, and they run no program. `None`
/// when git can't parse the file, which is then compared byte for byte. Includes are watched
/// as files of their own, so they aren't followed here. Only called on plain files.
fn config_entries(path: &Path) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(path)
        .args(["--no-includes", "--list", "--null"])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let mut kept = Vec::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if !entry.to_ascii_lowercase().starts_with(b"branch.") {
            kept.extend_from_slice(entry);
            kept.push(0);
        }
    }
    Some(kept)
}

/// The entries that differ between two [`config_entries`] listings, as `name: old -> new`,
/// `name added (value)` or `name removed`. Values show only in sections that hold no URLs or
/// credentials (`core.`, `extensions.` and the like), never when they look like a URL with a user
/// or a header, and are cut when long; a name with a URL in it shows only its section and key.
fn config_changes(before: &[u8], after: &[u8]) -> Vec<String> {
    fn entries(raw: &[u8]) -> Vec<(String, String)> {
        raw.split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(|e| {
                let e = String::from_utf8_lossy(e);
                let (name, value) = e.split_once('\n').unwrap_or((&e, ""));
                (name.to_string(), value.to_string())
            })
            .collect()
    }
    // Only sections whose values carry no URLs or secrets show them; anything that looks like
    // a URL with a user, or a header, stays hidden even there.
    fn shown(name: &str, value: &str) -> String {
        let lower = name.to_ascii_lowercase();
        let plain = [
            "core.",
            "extensions.",
            "safe.",
            "init.",
            "gc.",
            "index.",
            "feature.",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
            && !["password", "token", "secret", "key", "credential", "header"]
                .iter()
                .any(|word| lower.contains(word));
        let value_lower = value.to_ascii_lowercase();
        let risky = [
            "@",
            "://",
            "authorization",
            "bearer",
            "ghp_",
            "ghs_",
            "github_pat_",
        ]
        .iter()
        .any(|bit| value_lower.contains(bit));
        if !plain || risky {
            return "(value hidden)".into();
        }
        let mut cut: String = value.chars().take(80).collect();
        if value.chars().count() > 80 {
            cut.push('…');
        }
        format!("`{cut}`")
    }
    // A subsection can hold a URL with a user and password (`url.<url>.insteadof`).
    fn shown_name(name: &str) -> String {
        if name.contains('@') || name.contains("://") {
            let first = name.split('.').next().unwrap_or("");
            let last = name.rsplit('.').next().unwrap_or("");
            format!("{first}.(hidden).{last}")
        } else {
            name.to_string()
        }
    }
    let (was, now) = (entries(before), entries(after));
    let mut changes = Vec::new();
    let names: std::collections::BTreeSet<&str> = was
        .iter()
        .chain(&now)
        .map(|(name, _)| name.as_str())
        .collect();
    for name in names {
        let old: Vec<&str> = was
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        let new: Vec<&str> = now
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if old == new {
            continue;
        }
        let list = |values: &[&str]| -> String {
            values
                .iter()
                .map(|v| shown(name, v))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let shown_as = shown_name(name);
        let name = shown_as.as_str();
        changes.push(match (old.is_empty(), new.is_empty()) {
            (true, _) => format!("{name} added ({})", list(&new)),
            (_, true) => format!("{name} removed"),
            _ => format!("{name}: {} -> {}", list(&old), list(&new)),
        });
    }
    changes
}

/// A file's contents, read only when it is a plain file: opening a FIFO someone planted
/// would hang. Anything else reads as a marker that differs from every real content.
fn read_regular(path: &Path) -> Option<Vec<u8>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => std::fs::read(path).ok(),
        Ok(_) => Some(b"\0not a regular file\0".to_vec()),
        Err(_) => None,
    }
}

/// The paja binary the hook runs from: a copy in the factory home, so an agent's own
/// `cargo build` can't replace the binary that judges it. Made once per `paja run` from the
/// binary that is running (on Linux even if the file on disk has been rebuilt since; elsewhere
/// it is the file on disk when the run first needs it); copies other runs
/// left behind are removed once they are a day old.
fn hook_binary(home: &Home) -> Result<PathBuf> {
    let _lock = home.lock()?;
    let dir = home.root().join("hook");
    // Nobody else may swap the hook's binary: the directory is this user's alone.
    crate::platform::ensure_private_dir(&dir).with_context(|| {
        format!(
            "making {} private to this user for the deny-list hook",
            dir.display()
        )
    })?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let day = std::time::Duration::from_secs(24 * 60 * 60);
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let running =
        crate::platform::running_exe().context("finding the paja binary for the deny-list hook")?;
    let bytes = match RUNNING.get() {
        Some(bytes) => bytes,
        None => {
            let read = std::fs::read(&running)
                .with_context(|| format!("reading {} for the deny-list hook", running.display()))?;
            RUNNING.get_or_init(|| read)
        }
    };
    // One copy per build of paja, shared by every run of it, so copies don't pile up.
    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    // `.exe` on Windows, which runs nothing without it.
    let exe = dir.join(format!(
        "paja-{}-{digest:016x}{}",
        bytes.len(),
        std::env::consts::EXE_SUFFIX
    ));
    // The copy sits in the factory home, which code an agent runs could reach. Use it only
    // while it still matches this paja byte for byte; otherwise copy again.
    if !hook_copy_intact(&exe) {
        // Write under a temporary name and rename, so no run ever sees half a binary.
        let partial = dir.join(format!(
            "partial-{}-{}",
            std::process::id(),
            crate::events::now_ms()
        ));
        let written = (|| {
            let mut file = crate::platform::create_private_file(&partial)?;
            std::io::Write::write_all(&mut file, bytes)?;
            drop(file);
            std::fs::rename(&partial, &exe)
        })();
        written
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })
            .with_context(|| {
                format!(
                    "copying {} to {} for the deny-list hook; check that {} is writable",
                    running.display(),
                    exe.display(),
                    dir.display()
                )
            })?;
    }
    // Keep the copy fresh so another run's cleanup leaves it alone.
    crate::platform::touch(&exe);
    Ok(exe)
}

fn run_hook_once(command: &str, input: &str) -> Result<(Option<i32>, String)> {
    let mut child = crate::platform::shell(command)?
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting the deny-list hook command `{command}`"))?;
    // A hook that exits without reading its input is judged by its exit code, not the pipe.
    if let Err(err) = std::io::Write::write_all(
        child.stdin.as_mut().context("the hook's stdin")?,
        input.as_bytes(),
    ) {
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err).with_context(|| {
                format!(
                    "sending the self-test to the deny-list hook command `{command}`; rebuild \
                     paja from main"
                )
            });
        }
    }
    drop(child.stdin.take());
    // Drain stderr while waiting, so a chatty hook can't fill the pipe and look hung.
    let mut stderr = child.stderr.take().context("the hook's stderr")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    // The hook blocks itself after its deadline; anything longer is a hung binary.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(crate::hook::HOOK_DEADLINE_SECS + 10);
    while child
        .try_wait()
        .inspect_err(|_| {
            let _ = child.kill();
        })
        .with_context(|| format!("waiting for the deny-list hook command `{command}`"))?
        .is_none()
    {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the deny-list hook command `{command}` hung in its self-test, so no agent \
                 will start; rebuild paja from main"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let code = child
        .wait()
        .with_context(|| format!("waiting for the deny-list hook command `{command}`"))?
        .code();
    Ok((code, reader.join().unwrap_or_default()))
}

fn tempfile_dir(near: &Path) -> Result<PathBuf> {
    // A fresh directory only this user can enter; creating it fails if the name is taken, so
    // nobody can prepare it beforehand.
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "paja-agent-{}-{}-{}",
        std::process::id(),
        crate::events::now_ms(),
        COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(std::env::temp_dir());
    crate::platform::create_private_dir(&dir).with_context(|| {
        format!(
            "creating a scratch home {} for work in {}; check that the temporary \
                 directory (TMPDIR, or TEMP on Windows) exists and is writable",
            dir.display(),
            near.display()
        )
    })?;
    Ok(dir)
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

/// Give `cmd` a throwaway PAJA_HOME (`scratch`) and keep GitHub Mestari's: take away the easy
/// ways an agent, or the code it wrote, could reach it (tokens, the SSH agent, gh's stored
/// login, git's credential helpers and askpass programs, and registry tokens). Agents keep
/// the owner's `HOME`, which Claude Code needs to sign in, so code they run can still read
/// credential files there (`.netrc`, say), and a token written into the remote's URL is not
/// covered. Determined code run by `cargo test` is the sandbox's job (`PAJA_SBX`).
fn without_credentials(cmd: &mut Command, scratch: &Path, git_identity: Vec<(&str, String)>) {
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("GIT_CONFIG_")
            || name.starts_with("CARGO_REGISTRIES_") && name.ends_with("_TOKEN")
        {
            cmd.env_remove(name.as_ref());
        }
    }
    cmd.env("PAJA_HOME", scratch)
        // Replacement refs would let code show the reviewer a different history.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN")
        .env_remove("PAJA_GH")
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env("GH_CONFIG_DIR", scratch)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "false")
        .env("SSH_ASKPASS", "false")
        .env("GIT_SSH_COMMAND", "false")
        // Configuration from the environment comes last, and an empty helper clears the list
        // so far, so no credential helper from the repository's own config runs either.
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        // Without the global config, commits still need the owner's name and email.
        .envs(git_identity);
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

/// Push `judged`, the commit Vasara's checks and the reviewers saw, as the environment's
/// branch, and land it. Not whatever the branch points at now: code run in the worktree could
/// have moved it.
fn land(
    home: &Home,
    repo: &Path,
    env: &Env,
    item: &WorkItem,
    rounds: u32,
    judged: &str,
) -> Result<RunReport> {
    let gh = jono::gh_program();
    let branch = env.branch.as_str();
    git(
        &env.path,
        &[
            "push",
            "-q",
            // No `-u`: that would write the branch into the shared git configuration, which
            // other runs' guards watch.
            "origin",
            &format!("{judged}:refs/heads/{branch}"),
            &format!("--force-with-lease=refs/heads/{branch}"),
        ],
    )?;
    let title = format!("{} (#{})", item.title, item.number);
    let body = format!(
        "Closes #{}.\n\nBuilt and landed by Paja: Vasara's checks passed and an independent \
         reviewer agent approved it.",
        item.number
    );
    // Reuse this branch's pull request only while it is still open.
    let pr = match run_gh(
        repo,
        &[
            "pr",
            "view",
            branch,
            "--json",
            "url,state",
            "--jq",
            "select(.state == \"OPEN\") | .url",
        ],
    ) {
        Ok(url) if !url.is_empty() => url,
        _ => run_gh(
            repo,
            &[
                "pr", "create", "--base", "main", "--head", branch, "--title", &title, "--body",
                &body,
            ],
        )?,
    };
    // CI must be green before landing; a red CI is a failed check, not a merge.
    progress(&format!("opened {pr}; waiting for CI"));
    if let Err(err) = wait_for_ci(repo, &pr) {
        let question = format!("CI did not pass on {pr}, so Paja did not land it:\n\n{err:#}");
        return escalated(home, repo, env, item, &question, rounds);
    }
    // Free the worktree first: gh deletes the local branch too, which git refuses while a
    // worktree has it checked out.
    ahjo::remove(home, &env.name, true)?;
    run_gh(repo, &["pr", "merge", &pr, "--squash", "--delete-branch"])
        .with_context(|| format!("{gh} pr merge {pr}"))?;
    // Merged: from here nothing may turn this into an escalation.
    let _ = jono::set_status(home, item.number, Status::Landed, Some(&pr));
    let _ = record(
        home,
        EV_LANDED,
        json!({ "number": item.number, "pr": pr, "rounds": rounds }),
    );
    progress(&format!("landed #{} ({pr})", item.number));
    Ok(RunReport::Landed {
        number: item.number,
        pr,
        rounds,
    })
}

fn escalated(
    home: &Home,
    repo: &Path,
    env: &Env,
    item: &WorkItem,
    question: &str,
    rounds: u32,
) -> Result<RunReport> {
    // The branch keeps the work; the worktree and its ports are freed.
    let _ = ahjo::remove(home, &env.name, true);
    let failed = escalate(home, repo, item, question, rounds)?;
    progress(&parked_line(
        &format!("#{} needs a decision", item.number),
        item.number,
        failed,
    ));
    Ok(RunReport::Escalated {
        number: item.number,
        question: question.to_string(),
        rounds,
    })
}

/// The progress line for a parked item: where the question is and how to retry come first,
/// so a long gh error at the end can't push them out of the line.
fn parked_line(what: &str, number: u64, comment_error: Option<anyhow::Error>) -> String {
    let retry = format!("answer with `paja work answer {number} \"<your answer>\"`");
    match comment_error {
        None => format!("{what}; asked on the issue ({retry})"),
        Some(err) => format!(
            "{what}; couldn't comment on the issue, so see `paja work show {number}` for the \
             question ({retry}); gh said: {err:#}"
        ),
    }
}

/// Ask the person on the issue, and park the item until they answer. Returns the comment's
/// error when it didn't reach the issue.
fn escalate(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    question: &str,
    rounds: u32,
) -> Result<Option<anyhow::Error>> {
    // Park the item first, so it never stays active if the comment fails.
    jono::set_status(home, item.number, Status::Escalated, Some(question))?;
    record(
        home,
        EV_ESCALATED,
        json!({ "number": item.number, "rounds": rounds }),
    )?;
    // The item is parked either way; a failed comment must not escalate it a second time.
    // The caller reports it, cleaned for the terminal.
    Ok(comment(repo, item, question).err())
}

/// The label follow-up issues get. Jono imports only `paja`, so a follow-up waits until a
/// person relabels it: reviewers suggest work, people set intent.
pub const FOLLOW_UP_LABEL: &str = "paja-follow-up";

/// At most this many follow-up issues per change; the rest are folded into the last one.
const MAX_FOLLOW_UPS: usize = 5;

/// File each non-blocking review finding as a `paja-follow-up` issue, once the change has
/// landed. A failure here is recorded, never turned into an escalation: the change is already
/// merged.
fn file_follow_ups(home: &Home, repo: &Path, item: &WorkItem, pr: &str, findings: &[String]) {
    // Review text came from an agent that read untrusted code: keep each finding short, and
    // never let it mention anyone.
    let mut findings: Vec<String> = findings
        .iter()
        .map(|finding| {
            let mut text: String = finding.chars().take(4000).collect();
            if text.len() < finding.len() {
                text.push_str("\n\n(cut short)");
            }
            text.replace('@', "@\u{200b}")
        })
        .collect();
    if findings.len() > MAX_FOLLOW_UPS {
        let rest = findings.split_off(MAX_FOLLOW_UPS - 1);
        findings.push(format!(
            "More review follow-ups for #{}\n\n{}",
            item.number,
            rest.join("\n\n---\n\n")
        ));
    }
    for finding in &findings {
        let title: String = finding
            .lines()
            .map(|line| {
                line.trim()
                    .trim_matches(['#', '-', '*', '_', '`', ' '])
                    .trim()
            })
            .find(|line| !line.is_empty())
            .unwrap_or("Review follow-up")
            .chars()
            .take(100)
            .collect();
        let body = format!(
            "{finding}\n\nA reviewer found this while reviewing #{number} ({pr}). It did not \
             block landing. Relabel it `paja` to queue it.",
            number = item.number
        );
        let filed = run_gh(
            repo,
            &[
                "issue",
                "create",
                "--label",
                FOLLOW_UP_LABEL,
                "--title",
                &title,
                "--body",
                &body,
            ],
        );
        match &filed {
            Ok(issue) => record_follow_up(home, item, finding, Some(issue.as_str()), ""),
            Err(err) => record_follow_up(
                home,
                item,
                finding,
                None,
                &format!(
                    "{err:#}; if the label is missing, create it with `gh label create \
                     {FOLLOW_UP_LABEL}` and file this finding by hand"
                ),
            ),
        }
    }
}

/// Keep a follow-up in the log, filed or not, so no finding is lost.
fn record_follow_up(home: &Home, item: &WorkItem, finding: &str, issue: Option<&str>, error: &str) {
    let _ = record(
        home,
        EV_FOLLOW_UP,
        json!({
            "number": item.number,
            "finding": finding,
            "issue": issue,
            "error": (!error.is_empty()).then_some(error),
        }),
    );
}

/// Ask the person on the issue.
fn comment(repo: &Path, item: &WorkItem, question: &str) -> Result<()> {
    let body = format!(
        "**Paja needs a decision on this issue.**\n\n{question}\n\nAnswer with \
         `paja work answer {n} \"<your answer>\"` (or `/paja:answer` in Claude Code). That records \
         the answer for Paja's next run and queues the item again; a reply here alone doesn't \
         reach the next run.",
        n = item.number
    );
    run_gh(
        repo,
        &[
            "issue",
            "comment",
            &item.number.to_string(),
            "--body",
            &body,
        ],
    )?;
    Ok(())
}

/// Wait until GitHub has registered checks on the pull request, then until they finish.
/// A pull request that gets no checks at all is not treated as green.
fn wait_for_ci(repo: &Path, pr: &str) -> Result<()> {
    const APPEAR: std::time::Duration = std::time::Duration::from_secs(300);
    let start = std::time::Instant::now();
    loop {
        match run_gh(repo, &["pr", "checks", pr, "--watch", "--fail-fast"]) {
            Ok(_) => return Ok(()),
            Err(err) if format!("{err:#}").contains("no checks reported") => {
                if start.elapsed() > APPEAR {
                    bail!("no CI checks appeared on {pr} within 5 minutes; check the workflows");
                }
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
            Err(err) => return Err(err),
        }
    }
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

/// A snapshot of the git configuration, hooks and attributes a worktree's git reads, all of
/// them outside the worktree and shared with the repository. Code that an agent, Vasara's
/// checks or a reviewer's build runs could change them to make Mestari's own git run a program
/// with Mestari's credentials (a credential helper, a filter). Mestari takes the snapshot before
/// such code runs and checks it before it runs git there again.
struct GitGuard {
    files: Vec<PathBuf>,
    /// Watched files that are not git configuration, compared byte for byte.
    raw: Vec<PathBuf>,
    hooks: Vec<PathBuf>,
    snapshot: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Every other worktree's links and configuration: code here could repoint another
    /// environment's git. Those come and go as other runs work, so only a file that still
    /// exists and now differs counts.
    others: Vec<PathBuf>,
    others_snapshot: Vec<Option<Vec<u8>>>,
}

impl GitGuard {
    fn new(worktree: &Path) -> Result<GitGuard> {
        let dir = |arg: &str| -> Result<PathBuf> {
            let path = PathBuf::from(git(worktree, &["rev-parse", arg])?);
            Ok(if path.is_absolute() {
                path
            } else {
                worktree.join(path)
            })
        };
        let (own, common) = (dir("--absolute-git-dir")?, dir("--git-common-dir")?);
        // The repository's hooks, and wherever the owner's `core.hooksPath` points: gh runs
        // git without Paja's overrides.
        let mut hooks = vec![common.join("hooks")];
        // A relative path in the configuration is relative to where git runs: gh runs it in the
        // main checkout, next to the shared git directory, not in this worktree.
        let main_checkout = common.parent().unwrap_or(&common).to_path_buf();
        // Asked without Paja's own override, which would answer `/dev/null`. Reading the
        // configuration runs no hook.
        let owner_hooks = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["config", "--path", "core.hooksPath"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if !owner_hooks.is_empty() {
            hooks.push(main_checkout.join(owner_hooks));
        }
        // The owner's global configuration, which Mestari's own git reads.
        let home = crate::platform::home_dir();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let global: Vec<PathBuf> = home
            .iter()
            .map(|h| h.join(".gitconfig"))
            .chain(
                xdg.iter()
                    .flat_map(|x| [x.join("git/config"), x.join("git/attributes")]),
            )
            .chain(std::env::var_os("GIT_CONFIG_GLOBAL").map(PathBuf::from))
            .collect();
        let mut guard = GitGuard {
            // The worktree's links to the repository too: rewriting them would point git at
            // another configuration without touching these files.
            files: vec![
                common.join("config"),
                common.join("config.worktree"),
                own.join("config.worktree"),
                common.join("info/attributes"),
                common.join("info/grafts"),
                worktree.join(".git"),
                own.join("commondir"),
                own.join("gitdir"),
                common.join("info/exclude"),
                // Sparse checkout's patterns decide which files may be skip-worktree; new
                // ones could hide a test from the checks.
                own.join("info/sparse-checkout"),
            ],
            raw: Vec::new(),
            hooks,
            snapshot: Vec::new(),
            others: Vec::new(),
            others_snapshot: Vec::new(),
        };
        guard.files.extend(global);
        guard.raw = guard
            .files
            .iter()
            .filter(|path| {
                !(path.ends_with("config")
                    || path.ends_with("config.worktree")
                    || path.ends_with(".gitconfig"))
            })
            .cloned()
            .collect();
        // A global attributes file the owner chose, wherever it is.
        let attributes = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["config", "--path", "core.attributesFile"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if !attributes.is_empty() {
            // Mestari's own git reads a relative one here, gh's in the main checkout.
            for base in [worktree, main_checkout.as_path()] {
                let path = base.join(&attributes);
                // Attributes are compared byte for byte, even in a file also watched as
                // configuration: their lines aren't config entries.
                if !guard.raw.contains(&path) {
                    guard.raw.push(path.clone());
                }
                if !guard.files.contains(&path) {
                    guard.files.push(path);
                }
            }
        }
        // Every file git reads configuration from here, includes and the system file among
        // them, and every file an include names, read or not: one that doesn't exist yet
        // could be created.
        let origins = git(
            worktree,
            &["config", "--list", "--show-origin", "--name-only"],
        )?;
        for line in origins.lines() {
            if let Some(file) = line
                .strip_prefix("file:")
                .and_then(|l| l.split('\t').next())
            {
                let path = Path::new(file);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    worktree.join(path)
                };
                if !guard.files.contains(&path) {
                    guard.files.push(path);
                }
            }
        }
        let includes = git(
            worktree,
            &[
                "config",
                "--path",
                "--null",
                "--get-regexp",
                r"^include(if\..*)?\.path$",
            ],
        )
        .unwrap_or_default();
        for entry in includes.split('\0') {
            if let Some((_, file)) = entry.split_once('\n') {
                let path = PathBuf::from(file);
                // A relative include is relative to the file that names it: the repository's
                // config or the owner's global one, so watch it against both.
                let candidates: Vec<PathBuf> = if path.is_absolute() {
                    vec![path]
                } else {
                    [Some(common.clone()), home.clone()]
                        .into_iter()
                        .flatten()
                        .map(|base| base.join(&path))
                        .collect()
                };
                for path in candidates {
                    if !guard.files.contains(&path) {
                        guard.files.push(path);
                    }
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
            for entry in entries.flatten() {
                let dir = entry.path();
                for name in ["gitdir", "commondir", "config.worktree"] {
                    guard.others.push(dir.join(name));
                }
                // `gitdir` names the worktree's `.git` file, which points back here.
                if let Some(link) = read_regular(&dir.join("gitdir")) {
                    let link = String::from_utf8_lossy(&link).trim().to_string();
                    if !link.is_empty() {
                        guard.others.push(PathBuf::from(link));
                    }
                }
            }
        }
        guard.others.retain(|path| !guard.files.contains(path));
        guard.others_snapshot = guard.others.iter().map(|p| read_regular(p)).collect();
        guard.snapshot = guard.read();
        Ok(guard)
    }

    fn read(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut hooks: Vec<PathBuf> = self
            .hooks
            .iter()
            .flat_map(|dir| {
                std::fs::read_dir(dir)
                    .map(|entries| entries.flatten().map(|e| e.path()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect();
        hooks.sort();
        self.files
            .iter()
            .chain(&hooks)
            .map(|path| {
                let raw = read_regular(path);
                let content = if self.raw.contains(path) || !self.files.contains(path) {
                    raw
                } else {
                    raw.map(|bytes| config_entries(path).unwrap_or(bytes))
                };
                (path.clone(), content)
            })
            .collect()
    }

    /// Fails, before Mestari runs git again, when anything changed since the snapshot.
    fn check(&self, what: &str) -> Result<()> {
        let now = self.read();
        let others: Vec<String> = self
            .others
            .iter()
            .zip(&self.others_snapshot)
            .filter(|(path, before)| {
                let after = read_regular(path);
                after.is_some() && after != **before
            })
            .map(|(path, _)| path.display().to_string())
            .collect();
        if now != self.snapshot || !others.is_empty() {
            let before = |path: &PathBuf| self.snapshot.iter().find(|(p, _)| p == path);
            let mut changed: Vec<String> = now
                .iter()
                .filter(|entry| !self.snapshot.contains(entry))
                .chain(self.snapshot.iter().filter(|entry| !now.contains(entry)))
                .map(|(path, _)| path)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|path| {
                    let shown = path.display().to_string();
                    // A configuration file says which entries changed, so a person can tell
                    // what wrote it.
                    let after = now
                        .iter()
                        .find(|(p, _)| p == path)
                        .and_then(|(_, c)| c.as_ref());
                    let was = before(path).and_then(|(_, c)| c.as_ref());
                    match (was, after) {
                        (Some(was), Some(after))
                            if self.files.contains(path) && !self.raw.contains(path) =>
                        {
                            let entries = config_changes(was, after);
                            if entries.is_empty() {
                                shown
                            } else {
                                format!("{shown}: {}", entries.join("; "))
                            }
                        }
                        (None, Some(_)) => format!("{shown}: added"),
                        (Some(_), None) => format!("{shown}: removed"),
                        _ => shown,
                    }
                })
                .chain(others)
                .collect();
            changed.sort();
            changed.dedup();
            return Err(Tampered(format!(
                "git's configuration, hooks or attributes changed while {what} ran ({}), so \
                 Paja stopped before running git there again; a person must check and restore \
                 those files before running Paja again. The agent may have changed them, or \
                 another program writing to the same repository meanwhile (an editor, another \
                 session, a `git worktree add`), or your own edits",
                changed.join(", ")
            ))
            .into());
        }
        Ok(())
    }
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
    fn a_broken_hook_command_fails_the_self_test_and_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        // The command Claude Code would run, pointing at a hook binary that isn't there.
        let command = hook_command(
            &tmp.path().join("no-such-paja"),
            tmp.path(),
            tmp.path(),
            &tmp.path().join("stamp"),
            false,
        )
        .unwrap();
        let why = self_test_hook(&command, tmp.path(), false)
            .unwrap_err()
            .to_string();
        assert!(why.contains("failed its self-test"), "{why}");
        assert!(why.contains(&command), "{why}");
    }

    #[test]
    fn lines_skip_blanks_and_comments() {
        assert_eq!(
            lines("# checks\ncargo test\n\n  cargo fmt --all --check \n"),
            ["cargo test", "cargo fmt --all --check"]
        );
    }

    #[test]
    fn progress_lines_cannot_carry_terminal_controls() {
        let line = shown_line("Fix\r\x1b[2Kpaja: landed #5\nnext\u{2028}\u{202E}x\u{2067}y");
        assert!(!line.chars().any(unsafe_on_a_terminal), "{line:?}");
        assert!(shown_line(&"x".repeat(1000)).chars().count() <= 303);
    }

    #[test]
    fn config_changes_name_each_entry_and_hide_credentials() {
        let listing = |entries: &[(&str, &str)]| -> Vec<u8> {
            entries
                .iter()
                .flat_map(|(n, v)| format!("{n}\n{v}\0").into_bytes())
                .collect()
        };
        let before = listing(&[
            ("core.repositoryformatversion", "0"),
            ("remote.origin.url", "https://example.invalid/r"),
            ("http.extraheader", "Authorization: old"),
        ]);
        let after = listing(&[
            ("core.repositoryformatversion", "1"),
            ("extensions.relativeworktrees", "true"),
            ("http.extraheader", "Authorization: new"),
        ]);
        let changes = config_changes(&before, &after);
        assert_eq!(
            changes,
            [
                "core.repositoryformatversion: `0` -> `1`",
                "extensions.relativeworktrees added (`true`)",
                "http.extraheader: (value hidden) -> (value hidden)",
                "remote.origin.url removed",
            ]
        );
        // No credential reaches the message, in a value or in a name.
        let secret = "ghs_s3cr3t";
        let with_url = listing(&[
            (
                "remote.origin.url",
                &format!("https://x-access-token:{secret}@example.invalid/r"),
            ),
            (
                "url.https://u:pw@example.invalid/.insteadof",
                "https://example.invalid/",
            ),
            ("core.sshcommand", &format!("ssh -o User=me@{secret}")),
        ]);
        let changes = config_changes(&before, &with_url).join("; ");
        assert!(
            !changes.contains(secret) && !changes.contains("pw@"),
            "{changes}"
        );
        assert!(
            changes.contains("url.(hidden).insteadof added"),
            "{changes}"
        );
        assert!(config_changes(&before, &before).is_empty());
        let long = listing(&[("core.editor", &"x".repeat(200))]);
        let cut = config_changes(&[], &long);
        assert!(cut[0].ends_with("…`)"), "{cut:?}");
    }

    #[test]
    fn a_tamper_stop_names_the_configuration_entries_that_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", tail(&out));
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=paja",
            "-c",
            "user.email=paja@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ]);
        let worktree = tmp.path().join("work");
        run(&["worktree", "add", "-q", worktree.to_str().unwrap()]);
        let guard = GitGuard::new(&worktree).unwrap();
        guard.check("nothing").unwrap();
        // A branch entry is the agent's own business and still doesn't count.
        run(&["config", "branch.x.remote", "origin"]);
        guard.check("nothing").unwrap();
        run(&["config", "extensions.example", "true"]);
        let err = format!("{:#}", guard.check("the agent").unwrap_err());
        assert!(err.contains("extensions.example added (`true`)"), "{err}");
        assert!(!err.contains("branch.x"), "{err}");
    }

    #[test]
    fn guard_watches_a_relative_attributes_file_in_the_main_checkout() {
        // Removed when the test ends, passing or not.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", tail(&out));
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=paja",
            "-c",
            "user.email=paja@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ]);
        let worktree = dir.join("work");
        run(&["worktree", "add", "-q", worktree.to_str().unwrap()]);
        run(&["config", "core.attributesFile", "attrs"]);
        std::fs::write(main.join("attrs"), "").unwrap();
        std::fs::write(worktree.join("attrs"), "").unwrap();
        let guard = GitGuard::new(&worktree).unwrap();
        let canonical = |p: &Path| std::fs::canonicalize(p).ok();
        let watched: Vec<_> = guard.raw.iter().filter_map(|p| canonical(p)).collect();
        for expected in [main.join("attrs"), worktree.join("attrs")] {
            assert!(
                watched.contains(&canonical(&expected).unwrap()),
                "{} not in {watched:?}",
                expected.display()
            );
        }
        // An attributes file that is also watched as configuration is still compared byte
        // for byte.
        // Spelled as git spells the common directory, so it is the very path the guard
        // already watches as configuration.
        let common = git(
            &worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap();
        let config = format!("{common}/config");
        run(&["config", "core.attributesFile", &config]);
        let guard = GitGuard::new(&worktree).unwrap();
        assert!(guard.files.contains(&PathBuf::from(&common).join("config")));
        let watched: Vec<_> = guard.raw.iter().filter_map(|p| canonical(p)).collect();
        assert!(
            watched.contains(&canonical(Path::new(&config)).unwrap()),
            "{watched:?}"
        );
    }

    #[test]
    fn the_builder_asks_only_on_a_line_of_its_own() {
        let ask = |text: &str| builder_question(&json!({ "result": text }));
        assert_eq!(
            ask("Stuck.\nPAJA-QUESTION: A or B? I'd pick A.\nThanks").as_deref(),
            Some("A or B? I'd pick A.\nThanks")
        );
        assert_eq!(ask("PAJA-QUESTION: Which?").as_deref(), Some("Which?"));
        assert_eq!(ask("Done. No PAJA-QUESTION: here."), None);
        assert_eq!(ask("PAJA-QUESTION:   \n"), None);
        assert_eq!(builder_question(&json!({})), None);
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
        let prompt = super::review_prompt(&item, "base1", "abc123", None);
        assert!(prompt.contains("already passed at abc123"), "{prompt}");
        assert!(prompt.contains("don't run `cargo fmt`, `cargo clippy` or the whole `cargo test`"));
        assert!(prompt.contains("`cargo test <name>`"));
        // A re-review judges only the fix, at the checked commit, against the earlier blockers.
        let prior = super::Prior {
            reviewed: "old456".into(),
            blockers: "Fix the parser.".into(),
            follow_ups: vec!["Tidy the helper".into()],
        };
        let prompt = super::review_prompt(&item, "base1", "abc123", Some(&prior));
        assert!(prompt.contains("already passed at abc123"), "{prompt}");
        assert!(prompt.contains("`git diff old456..HEAD`"), "{prompt}");
        assert!(prompt.contains("Fix the parser."), "{prompt}");
        assert!(prompt.contains("Tidy the helper"), "{prompt}");
        assert!(!prompt.contains("git diff base1...HEAD"), "{prompt}");
    }

    #[test]
    fn a_resumed_step_counts_only_its_own_spend() {
        use super::{Models, StepUsage};
        let none = Models::default();
        let first = StepUsage::from_result(
            "build",
            None,
            &none,
            "build",
            &serde_json::json!({"total_cost_usd": 0.5, "usage": {"input_tokens": 10},
                "modelUsage": {"m": {"inputTokens": 10, "costUSD": 0.5}}}),
        );
        let later = StepUsage::from_result(
            "build",
            None,
            &none,
            "fix",
            &serde_json::json!({"total_cost_usd": 0.75, "usage": {"input_tokens": 4},
                "modelUsage": {"m": {"inputTokens": 14, "costUSD": 0.75}}}),
        );
        let step = later.since(&first);
        assert!(step.resumed);
        assert_eq!(step.cost_usd, Some(0.25));
        // `usage` already counts only this run; `modelUsage` is the session's running total.
        assert_eq!(step.input_tokens, Some(4));
        assert_eq!(step.models["m"].input_tokens, Some(4));
        assert_eq!(step.models["m"].cost_usd, Some(0.25));
        // An earlier total that is unknown leaves this step's cost unknown, never a guess.
        let unknown = StepUsage::from_result("build", None, &none, "build", &serde_json::json!({}));
        assert_eq!(later.since(&unknown).cost_usd, None);
    }

    #[test]
    fn a_step_says_when_the_fallback_model_served_it() {
        use super::{Models, StepUsage};
        let models = Models::parse("build opus\nfix sonnet\nfallback sonnet\n").unwrap();
        let result = |sonnet_cost: f64| {
            serde_json::json!({"modelUsage": {
                "claude-opus-x": {"outputTokens": 5, "costUSD": 0.5},
                "claude-sonnet-x": {"outputTokens": 0, "costUSD": sonnet_cost}}})
        };
        let opus_only = StepUsage::from_result("build", None, &models, "build", &result(0.0));
        assert_eq!(opus_only.fallback.as_deref(), Some("sonnet"));
        assert!(!opus_only.fell_back);
        let fell = StepUsage::from_result("build", None, &models, "build", &result(0.1));
        assert!(fell.fell_back);
        // A role already on the fallback model gets none, so it never falls back.
        let fix = StepUsage::from_result("build", None, &models, "fix", &result(0.1));
        assert_eq!(fix.fallback, None);
        assert!(!fix.fell_back);
        // A resumed step is judged on its own share: the earlier fallback doesn't carry over.
        let later = StepUsage::from_result("build", None, &models, "build", &result(0.1));
        assert!(!later.since(&fell).fell_back);
        assert!(later.since(&opus_only).fell_back);
    }
}
