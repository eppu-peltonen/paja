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
}

pub struct RunOptions<'a> {
    /// Any path inside the repository's main checkout.
    pub repo: &'a Path,
}

/// `claude`, or the program named by `PAJA_CLAUDE` (tests point it at a fake).
fn claude_program() -> String {
    std::env::var("PAJA_CLAUDE").unwrap_or_else(|_| "claude".to_string())
}

/// Take the next work item and run it to an outcome.
pub fn run_once(home: &Home, opts: RunOptions<'_>) -> Result<RunReport> {
    let repo = std::path::absolute(opts.repo)?;
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
        if let Some(failure) = run_checks(&env.path, &checks)? {
            feedback =
                format!("Vasara's checks failed. Fix the cause, not the check:\n\n{failure}");
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
                &format!("{base}...HEAD"),
            ],
        )?;
        let risky = changed.lines().any(|path| {
            high_risk
                .iter()
                .any(|prefix| path.starts_with(prefix.as_str()))
        });
        let reviewers = if risky { 2 } else { 1 };
        let reviews = (0..reviewers)
            .map(|_| review(home, &env, item, base))
            .collect::<Result<Vec<_>>>()?;
        let verdicts: Vec<&str> = reviews.iter().map(|r| r.verdict.as_str()).collect();
        record_round(home, item, round, &verdicts.join(","), &summaries(&reviews))?;

        if let Some(esc) = reviews.iter().find(|r| r.verdict == "escalate") {
            return escalated(home, repo, &env, item, &esc.question, round);
        }
        if verdicts.iter().all(|v| *v == "land") {
            return land(home, repo, &env, item, round);
        }
        if risky && verdicts.contains(&"land") {
            // High-risk reviewers must agree; a split is a question, not another round.
            let question = format!(
                "The two reviewers disagree on this high-risk change.\n\n{}",
                summaries(&reviews)
            );
            return escalated(home, repo, &env, item, &question, round);
        }
        feedback = format!(
            "A reviewer asked for changes:\n\n{}",
            reviews
                .iter()
                .filter(|r| r.verdict == "changes")
                .map(|r| r.comments.as_str())
                .collect::<Vec<_>>()
                .join("\n\n")
        );
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
         and commit your work on the current branch (never push, never switch branches). End the \
         commit message with `Closes #{number}`.\n\nIssue #{number}: {title}\n\n{body}\n",
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
        "Bash(git status*)",
        "Bash(git diff*)",
        "Bash(git log*)",
    ];
    claude(home, env, &prompt, &tools, None).map(|_| ())
}

fn review(home: &Home, env: &Env, item: &WorkItem, base: &str) -> Result<Review> {
    let prompt = format!(
        "You are an independent reviewer for Paja. You did not write this change. Read it cold \
         and decide whether it lands.\n\nThe change is `git diff {base}...HEAD`. Judge it against \
         the issue below and the rules in CLAUDE.md as they are on main (`git show {base}:CLAUDE.md`), \
         not the branch's copy. Verdicts: `land` if it does what the issue asks, is correct and \
         tested, and breaks no rule; `changes` with concrete comments if it needs work; `escalate` \
         with a question only when the issue's intent is unclear or the change would weaken a check \
         (a person must decide). Nits alone are not a reason for `changes`.\n\n\
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
            "question": {"type": "string"}
        },
        "required": ["verdict", "summary"]
    });
    let tools = [
        "Read",
        "Glob",
        "Grep",
        "Bash(git diff*)",
        "Bash(git log*)",
        "Bash(git show*)",
    ];
    let out = claude(home, env, &prompt, &tools, Some(&schema))?;
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
    prompt: &str,
    tools: &[&str],
    schema: Option<&Value>,
) -> Result<Value> {
    let program = claude_program();
    // Agents get a throwaway PAJA_HOME, so trying the CLI never touches the factory's state.
    let scratch = tempfile_dir(&env.path)?;
    let settings = hook_settings(&env.path, home.root())?;
    let mut cmd = Command::new(&program);
    cmd.current_dir(&env.path)
        .env("PAJA_HOME", &scratch)
        .args([
            "-p",
            prompt,
            "--output-format",
            "json",
            "--permission-mode",
            "acceptEdits",
        ])
        // No settings files at all, only the deny-list hook: neither the owner's settings nor
        // a settings file on the branch can loosen or switch it off. CLAUDE.md still loads.
        .args(["--setting-sources", "", "--settings", &settings])
        .arg("--allowedTools")
        .args(tools);
    if let Some(schema) = schema {
        cmd.args(["--json-schema", &schema.to_string()]);
    }
    let output = cmd.stdin(std::process::Stdio::null()).output();
    let _ = std::fs::remove_dir_all(&scratch);
    let output =
        output.with_context(|| format!("running `{program}` (install Claude Code and sign in)"))?;
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

/// Claude Code settings that run every tool call through `paja hook`, bounded to `root`.
/// Before handing them out, check that the hook really blocks a push and allows a build; an
/// agent never starts under a hook that doesn't.
fn hook_settings(root: &Path, home: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("finding the paja binary for the deny-list hook")?;
    let args = [
        "hook".into(),
        "--root".into(),
        root.as_os_str().to_owned(),
        "--home".into(),
        home.as_os_str().to_owned(),
    ];
    let (push, build) = crate::hook::self_test_inputs(root);
    let (code, stderr) = run_hook_once(&exe, &args, &push)?;
    let blocked = code == Some(2) && stderr.contains("Blocked by Paja's deny-list");
    let (code, _) = run_hook_once(&exe, &args, &build)?;
    if !blocked || code != Some(0) {
        bail!(
            "the deny-list hook {} failed its self-test (it must block `git push` and allow \
             `cargo test`), so no agent will start; rebuild paja from main",
            exe.display()
        );
    }
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let command = format!(
        "{} hook --root {} --home {}",
        quote(&exe.to_string_lossy()),
        quote(&root.to_string_lossy()),
        quote(&home.to_string_lossy())
    );
    Ok(json!({
        "hooks": {
            "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": command}]}]
        }
    })
    .to_string())
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
    std::io::Write::write_all(
        child.stdin.as_mut().context("the hook's stdin")?,
        input.as_bytes(),
    )?;
    drop(child.stdin.take());
    let out = child.wait_with_output()?;
    Ok((
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn tempfile_dir(near: &Path) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "paja-agent-{}-{}",
        std::process::id(),
        crate::events::now_ms()
    ));
    std::fs::create_dir_all(&dir).with_context(|| {
        format!(
            "creating a scratch home for the agent in {}",
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

/// Run each check in the environment. Returns the first failure's command and output.
fn run_checks(dir: &Path, checks: &[String]) -> Result<Option<String>> {
    for check in checks {
        let output = Command::new("sh")
            .args(["-c", check])
            .current_dir(dir)
            .output()
            .with_context(|| format!("running check `{check}`"))?;
        if !output.status.success() {
            return Ok(Some(format!("`{check}` failed:\n\n{}", tail(&output))));
        }
    }
    Ok(None)
}

fn land(home: &Home, repo: &Path, env: &Env, item: &WorkItem, rounds: u32) -> Result<RunReport> {
    let gh = jono::gh_program();
    let branch = env.branch.as_str();
    git(
        &env.path,
        &[
            "push",
            "-q",
            "-u",
            "origin",
            &format!("{branch}:{branch}"),
            "--force-with-lease",
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
    let output = Command::new("git")
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

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
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
