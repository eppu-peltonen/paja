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

pub const EV_STARTED: &str = "run.started";
pub const EV_ROUND: &str = "run.round";
pub const EV_LANDED: &str = "run.landed";
pub const EV_ESCALATED: &str = "run.escalated";
/// A non-blocking review finding filed as a `paja-follow-up` issue after landing.
pub const EV_FOLLOW_UP: &str = "run.follow_up";
/// Branch code changed git's configuration in `repo`; Paja runs no git there until a person
/// has checked it and recorded [`EV_TAMPER_CHECKED`].
pub const EV_TAMPERED: &str = "run.tampered";
pub const EV_TAMPER_CHECKED: &str = "run.tamper_checked";

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

/// Take the next work item and run it to an outcome.
pub fn run_once(home: &Home, opts: RunOptions<'_>) -> Result<RunReport> {
    let repo = std::path::absolute(opts.repo)?;
    // A repository whose configuration was tampered with gets no git, not even a fetch, until
    // a person says it was checked.
    // One key per repository, however its path was spelled: the shared git directory, which
    // rev-parse finds without running any program from the configuration.
    let common = git(
        &repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .with_context(|| format!("finding the git repository at {}", repo.display()))?;
    let key = std::fs::canonicalize(&common)
        .unwrap_or_else(|_| PathBuf::from(&common))
        .display()
        .to_string();
    if opts.tamper_checked {
        record(home, EV_TAMPER_CHECKED, json!({ "repo": key }))?;
    }
    let last =
        home.events().read_all()?.into_iter().rev().find(|e| {
            (e.kind == EV_TAMPERED || e.kind == EV_TAMPER_CHECKED) && e.data["repo"] == key
        });
    if let Some(event) = last.filter(|e| e.kind == EV_TAMPERED) {
        return Err(Tampered(format!(
            "Paja stopped on {key} because git's configuration changed during a run ({}); it \
             runs no git there until a person has checked and restored those files and runs \
             `paja run --tamper-checked`",
            event.data["error"].as_str().unwrap_or("see the event log")
        ))
        .into());
    }
    git(&repo, &["fetch", "-q", "origin", "main"]).context(
        "fetching origin/main; check the network and that `origin` is the GitHub repository",
    )?;
    let base = git(&repo, &["rev-parse", "--verify", "FETCH_HEAD^{commit}"])?;
    jono::sync(home, &repo)?;
    let Some(item) = jono::claim(home)? else {
        return Ok(RunReport::Idle);
    };
    // Anything that goes wrong from here is reported on the issue, not lost.
    let mut rounds = 0;
    match run_item(home, &repo, &base, &item, &mut rounds) {
        Ok(report) => Ok(report),
        Err(err) => {
            let question = format!(
                "Paja's run stopped with an error it can't fix by itself:\n\n```\n{err:#}\n```\n\n\
                 Fix the cause, then set the item back to queued with `paja work set {} queued`.",
                item.number
            );
            // Park the item first, so it never stays active if GitHub is the thing failing.
            jono::set_status(home, item.number, Status::Escalated, Some(&question))?;
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
            let _ = comment(&repo, &item, &question);
            let _ = ahjo::remove(home, &format!("work-{}", item.number), true);
            Ok(RunReport::Escalated {
                number: item.number,
                question,
                rounds,
            })
        }
    }
}

fn run_item(
    home: &Home,
    repo: &Path,
    base: &str,
    item: &WorkItem,
    rounds: &mut u32,
) -> Result<RunReport> {
    let checks = read_from_base(repo, base, CHECKS_FILE)?.with_context(|| {
        format!("{CHECKS_FILE} is missing on main; add the check commands there")
    })?;
    let checks: Vec<String> = lines(&checks);
    if checks.is_empty() {
        // An empty gate would pass everything; that is never what main meant.
        bail!("{CHECKS_FILE} on main has no commands; add the check commands there");
    }
    let high_risk = lines(&read_from_base(repo, base, HIGH_RISK_FILE)?.unwrap_or_default());
    let env = environment(home, repo, base, item)?;
    record(
        home,
        EV_STARTED,
        json!({ "number": item.number, "env": env.name, "base": base }),
    )?;

    let mut feedback = String::new();
    let mut prior: Option<Prior> = None;
    let mut follow_ups: Vec<String> = Vec::new();
    for round in 1..=MAX_ROUNDS {
        *rounds = round;
        build(home, &env, item, base, &feedback)?;
        commit_leftovers(&env, item)?;
        if git(
            &env.path,
            &["rev-list", "--count", &format!("{base}..HEAD")],
        )? == "0"
        {
            feedback = "The previous round made no commits. Make the change and commit it.".into();
            record_round(home, item, round, "no_change", &feedback)?;
            continue;
        }
        // A round after a review must commit a fix; an empty fix is never reviewed.
        if let Some(prior) = &prior {
            if git(&env.path, &["rev-parse", "HEAD"])? == prior.reviewed {
                feedback = format!(
                    "The previous round made no new commits. Fix these blockers and commit:\n\n{}",
                    prior.blockers
                );
                record_round(home, item, round, "no_change", &feedback)?;
                continue;
            }
        }
        let checked = git(&env.path, &["rev-parse", "HEAD"])?;
        let guard = GitGuard::new(&env.path)?;
        let result = run_checks(&env.path, &checks);
        guard.check("Vasara's checks")?;
        // The checks run the branch's code, which could change the tree or even commit. Put it
        // back, and fail the round, so nothing lands that the checks didn't judge.
        let mut failed = result;
        // Files marked assume-unchanged or skip-worktree would hide from `git status`.
        let hidden = git(&env.path, &["ls-files", "-v"])?
            .lines()
            .any(|line| line.starts_with(|c: char| c.is_ascii_lowercase() || c == 'S'));
        let moved = git(&env.path, &["rev-parse", "HEAD"])? != checked
            || hidden
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
            record_round(home, item, round, "checks_failed", &failure)?;
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
        let reviews = (0..reviewers)
            // High-risk reviewers read the whole change every round: the two must agree on all
            // of it, not only on the fix.
            .map(|_| review(home, &env, item, base, prior.as_ref().filter(|_| !risky)))
            .collect::<Result<Vec<_>>>()?;
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
        record_round(home, item, round, &verdicts.join(","), &summaries(&reviews))?;

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
         Clarify the issue (what should change, or what is out of scope), then reopen it or run \
         `paja work set {} queued`.",
        item.number
    );
    escalated(home, repo, &env, item, &question, MAX_ROUNDS)
}

/// A fresh environment for the item, branched from the base. A branch kept from an earlier
/// run of the same item is resumed, so its work isn't thrown away.
fn environment(home: &Home, repo: &Path, base: &str, item: &WorkItem) -> Result<Env> {
    let name = format!("work-{}", item.number);
    if let Ok(env) = ahjo::get(home, &name) {
        return Ok(env);
    }
    let branch = format!("refs/heads/paja/{name}");
    let resume = git(repo, &["rev-parse", "--verify", "--quiet", &branch]).is_ok();
    ahjo::create(
        home,
        CreateOptions {
            name: &name,
            repo,
            base: if resume { None } else { Some(base) },
        },
    )
}

fn build(home: &Home, env: &Env, item: &WorkItem, base: &str, feedback: &str) -> Result<()> {
    let mut prompt = format!(
        "You are Paja's builder. Implement this GitHub issue in the current checkout, following \
         CLAUDE.md. Keep the change small and focused on the issue. Run the checks CLAUDE.md lists, \
         and commit your work on the current branch (never push, never switch branches). Commit \
         with `git add -A && git commit -m '<message>'`, the message one single-quoted argument \
         that ends with `Closes #{number}`. Read and change files with the file tools; Bash is \
         only for cargo and local git, and Paja's deny-list hook blocks anything else (no `$(`, \
         heredocs or redirects). When output is long, pipe it through `tail -n 80` or `grep`.\n\nIssue #{number}: {title}\n\n{body}\n",
        number = item.number,
        title = item.title,
        body = item.body
    );
    if !feedback.is_empty() {
        prompt.push_str(&format!(
            "\nThis is another round on the same branch (based on {base}). Address this first:\n\n{feedback}\n"
        ));
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
    claude(home, env, base, &prompt, &tools, None, false).map(|_| ())
}

fn review(
    home: &Home,
    env: &Env,
    item: &WorkItem,
    base: &str,
    prior: Option<&Prior>,
) -> Result<Review> {
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
    let prompt = format!(
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
         them. They never hold the change up. You may run cargo and read-only git; you \
         can't change files.\n\n\
         Issue #{number}: {title}\n\n{body}\n",
        number = item.number,
        title = item.title,
        body = item.body
    );
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
    let before = git(&env.path, &["rev-parse", "HEAD"])?;
    let out = claude(home, env, base, &prompt, &tools, Some(&schema), true);
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
    Ok(review)
}

/// Run headless Claude Code in the environment and return its JSON result.
fn claude(
    home: &Home,
    env: &Env,
    base: &str,
    prompt: &str,
    tools: &[&str],
    schema: Option<&Value>,
    read_only: bool,
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
    let (settings, stamp, exe) = match hook_settings(&env.path, home, read_only) {
        Ok(made) => made,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&scratch);
            return Err(err);
        }
    };
    let mut cmd = Command::new(&program);
    cmd.current_dir(&env.path);
    without_credentials(&mut cmd, &scratch, git_identity);
    cmd.args([
        "-p",
        prompt,
        "--output-format",
        "json",
        "--permission-mode",
        "acceptEdits",
    ])
    // No settings files at all, only the deny-list hook: neither the owner's settings nor
    // a settings file on the branch can loosen or switch it off.
    .args(["--setting-sources", "", "--settings", &settings])
    // With no setting sources Claude Code loads no CLAUDE.md, so pass the harness from
    // main: the branch under review can't rewrite the rules its agents follow.
    .args(["--append-system-prompt", &harness])
    // No MCP servers from anyone's configuration: agents get only Claude Code's own tools.
    .arg("--strict-mcp-config")
    .arg("--allowedTools")
    .args(tools);
    if let Some(schema) = schema {
        cmd.args(["--json-schema", &schema.to_string()]);
    }
    let before = git_head(&env.path);
    let status_before = git_status(&env.path);
    let output = cmd.stdin(std::process::Stdio::null()).output();
    let _ = std::fs::remove_dir_all(&scratch);
    let hooked = stamp.exists();
    let _ = std::fs::remove_file(&stamp);
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    guard.check("the agent")?;
    // Code the agent ran could also have swapped the hook's copy during the session.
    let hooked = hooked && hook_copy_intact(&exe);
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
        bail!(
            "Claude Code never called Paja's deny-list hook during the agent's run in {}, or the \
             hook's copy {} changed during it, so its work is not trusted and {fate}; check for \
             settings that disable hooks (such as `disableAllHooks` in managed settings) and run \
             again",
            env.path.display(),
            exe.display()
        );
    }
    let output = output.with_context(|| {
        format!(
            "running `{program}` (install Claude Code and sign in{})",
            crate::platform::CLAUDE_HINT
        )
    })?;
    if !output.status.success() {
        bail!(
            "`{program} -p` failed in {}: {}",
            env.path.display(),
            tail(&output)
        );
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("`{program} -p` printed something that is not JSON"))?;
    if value.get("is_error") == Some(&Value::Bool(true)) {
        bail!("`{program} -p` reported an error: {}", value["result"]);
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

/// Put the environment's branch, index and files back at `to` after code ran there. The
/// index is rebuilt from scratch, so no file stays hidden behind an assume-unchanged or
/// skip-worktree flag. Ignored files such as `target/` stay.
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
    let mut args: Vec<std::ffi::OsString> = vec![
        "hook".into(),
        "--root".into(),
        root.as_os_str().to_owned(),
        "--home".into(),
        home.root().as_os_str().to_owned(),
    ];
    if read_only {
        args.push("--read-only".into());
    }
    let (blocked, build) = crate::hook::self_test_inputs(root, read_only);
    let (code, stderr) = run_hook_once(&exe, &args, &blocked)?;
    let blocked = code == Some(2) && stderr.contains("Blocked by Paja's deny-list");
    let (code, _) = run_hook_once(&exe, &args, &build)?;
    if !blocked || code != Some(0) {
        bail!(
            "the deny-list hook {} failed its self-test (it must block `{}` and allow \
             `cargo test`), so no agent will start; rebuild paja from main",
            exe.display(),
            if read_only { "git commit" } else { "git push" }
        );
    }
    // The self-test leaves no stamp: the hook only gets `--stamp` from Claude Code. The stamp
    // sits in a fresh directory only this user can enter, so nobody can plant it beforehand;
    // creating the directory fails if the name is already taken.
    let private = std::env::temp_dir().join(format!(
        "paja-stamp-{}-{}-{}",
        std::process::id(),
        crate::events::now_ms(),
        STAMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    crate::platform::create_private_dir(&private)
        .with_context(|| format!("creating the hook's stamp directory {}", private.display()))?;
    let stamp = private.join("stamp");
    // Claude Code only blocks on exit 2. A hook that can't start (127), is killed or fails
    // in any other way must block too.
    let command = format!(
        "{} hook --root {} --home {} --stamp {}{} || exit 2",
        crate::platform::quote_hook_word(&exe)?,
        crate::platform::quote_hook_word(root)?,
        crate::platform::quote_hook_word(home.root())?,
        crate::platform::quote_hook_word(&stamp)?,
        if read_only { " --read-only" } else { "" }
    );
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
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    // Nobody else may swap the hook's binary, or rename its directory away: the factory home
    // and the hook directory are this user's alone.
    for private in [home.root(), dir.as_path()] {
        crate::platform::restrict_to_owner(private).with_context(|| {
            format!(
                "making {} private to this user for the deny-list hook",
                private.display()
            )
        })?;
    }
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

fn run_hook_once(
    exe: &Path,
    args: &[std::ffi::OsString],
    input: &str,
) -> Result<(Option<i32>, String)> {
    let mut child = Command::new(exe)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting the deny-list hook {}", exe.display()))?;
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
                    "sending the self-test to the deny-list hook {}; rebuild paja from main",
                    exe.display()
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
        .with_context(|| format!("waiting for the deny-list hook {}", exe.display()))?
        .is_none()
    {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the deny-list hook {} hung in its self-test, so no agent will start; rebuild \
                 paja from main",
                exe.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let code = child
        .wait()
        .with_context(|| format!("waiting for the deny-list hook {}", exe.display()))?
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
/// covered. Determined code run by `cargo test` is the sandbox's job.
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
    let result = (|| {
        for check in checks {
            let mut cmd = crate::platform::shell(check)?;
            cmd.current_dir(dir);
            without_credentials(&mut cmd, &scratch, identity.clone());
            // The checks keep the owner's HOME, as agents do: toolchain managers find cargo
            // through it. Credential files there stay reachable until the sandbox.
            let output = cmd
                .stdin(std::process::Stdio::null())
                .output()
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
    escalate(home, repo, item, question, rounds)?;
    Ok(RunReport::Escalated {
        number: item.number,
        question: question.to_string(),
        rounds,
    })
}

/// Ask the person on the issue, and park the item until they answer.
fn escalate(home: &Home, repo: &Path, item: &WorkItem, question: &str, rounds: u32) -> Result<()> {
    // Park the item first, so it never stays active if the comment fails.
    jono::set_status(home, item.number, Status::Escalated, Some(question))?;
    record(
        home,
        EV_ESCALATED,
        json!({ "number": item.number, "rounds": rounds }),
    )?;
    // The item is parked either way; a failed comment must not escalate it a second time.
    if let Err(err) = comment(repo, item, question) {
        eprintln!(
            "warning: couldn't comment on issue #{}: {err:#}",
            item.number
        );
    }
    Ok(())
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
        "**Paja needs a decision on this issue.**\n\n{question}\n\nAnswer here, then run \
         `paja work set {} queued` to try again.",
        item.number
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
) -> Result<()> {
    record(
        home,
        EV_ROUND,
        json!({ "number": item.number, "round": round, "verdict": verdict, "detail": clip(detail) }),
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
    let output = Command::new(&gh)
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

/// The error a [`GitGuard`] raises. Nothing may run git in that repository afterwards, not
/// even to clean up, until a person has looked.
#[derive(Debug)]
pub struct Tampered(String);

impl std::fmt::Display for Tampered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Tampered {}

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
            // A relative path is relative to where git runs: gh runs it in the main checkout,
            // next to the shared git directory, not in this worktree.
            let main_checkout = common.parent().unwrap_or(&common).to_path_buf();
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
            guard.files.push(worktree.join(&attributes));
            guard.raw.push(worktree.join(attributes));
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
            let mut changed: Vec<String> = now
                .iter()
                .filter(|entry| !self.snapshot.contains(entry))
                .chain(self.snapshot.iter().filter(|entry| !now.contains(entry)))
                .map(|(path, _)| path.display().to_string())
                .chain(others)
                .collect();
            changed.sort();
            changed.dedup();
            return Err(Tampered(format!(
                "git's configuration, hooks or attributes changed while {what} ran ({}), so \
                 Paja stopped before running git there again; a person must check and restore \
                 those files before running Paja again. Your own edits to them during a run \
                 stop it too",
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
}
