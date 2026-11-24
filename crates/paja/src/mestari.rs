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
    ];
    claude(home, env, base, &prompt, &tools, None, false).map(|_| ())
}

fn review(home: &Home, env: &Env, item: &WorkItem, base: &str) -> Result<Review> {
    let prompt = format!(
        "You are an independent reviewer for Paja. You did not write this change. Read it cold \
         and decide whether it lands.\n\nThe change is `git diff {base}...HEAD`. Judge it against \
         the issue below and the rules in CLAUDE.md as they are on main (`git show {base}:CLAUDE.md`), \
         not the branch's copy. Verdicts: `land` if it does what the issue asks, is correct and \
         tested, and breaks no rule; `changes` with concrete comments if it needs work; `escalate` \
         with a question only when the issue's intent is unclear or the change would weaken a check \
         (a person must decide). Nits alone are not a reason for `changes`. You may run cargo \
         and read-only git; you can't change files.\n\n\
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
        "Bash(cargo *)",
        "Bash(git status*)",
        "Bash(git diff*)",
        "Bash(git log*)",
        "Bash(git show*)",
    ];
    let before = git(&env.path, &["rev-parse", "HEAD"])?;
    let out = claude(home, env, base, &prompt, &tools, Some(&schema), true);
    // A reviewer changes nothing, but the code its builds and tests run can (a lockfile, even
    // a commit). Put the branch back where the review started, so none of it lands unread.
    // Ignored files such as `target/` stay.
    let reset = git(
        &env.path,
        &["checkout", "-q", "-f", "-B", env.branch.as_str(), &before],
    )
    .and_then(|_| git(&env.path, &["clean", "-q", "-f", "-d"]));
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
    // Agents get a throwaway PAJA_HOME, so trying the CLI never touches the factory's state.
    let scratch = tempfile_dir(&env.path)?;
    let (settings, stamp) = match hook_settings(&env.path, home, read_only) {
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
            "Claude Code never called Paja's deny-list hook during the agent's run in {}, so its \
             work is not trusted and {fate}; check for settings that disable hooks (such as \
             `disableAllHooks` in managed settings) and run again",
            env.path.display()
        );
    }
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

/// The owner's commit name and email, and their global ignore file, which agents' git uses
/// while git's global configuration is hidden from them.
fn git_identity(repo: &Path) -> Vec<(&'static str, String)> {
    let get = |key: &str| {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["config", "--path", key])
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
    let out = Command::new("git")
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
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Put the environment's own branch back at `to` and drop every file made since, ignored
/// ones included, even if the agent switched branches.
fn discard(env: &Env, to: &str) -> Result<()> {
    for args in [
        &["checkout", "-q", "-f", "-B", &env.branch, to][..],
        &["clean", "-q", "-f", "-d", "-x"],
    ] {
        let status = Command::new("git")
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

/// Claude Code settings that run every tool call through `paja hook`, bounded to `root`.
/// Before handing them out, check that the hook really blocks a push and allows a build; an
/// agent never starts under a hook that doesn't.
fn hook_settings(root: &Path, home: &Home, read_only: bool) -> Result<(String, PathBuf)> {
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
        "paja-stamp-{}-{}",
        std::process::id(),
        crate::events::now_ms()
    ));
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&private)
            .with_context(|| {
                format!("creating the hook's stamp directory {}", private.display())
            })?;
    }
    let stamp = private.join("stamp");
    let quote = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    // Claude Code only blocks on exit 2. A hook that can't start (127), is killed or fails
    // in any other way must block too.
    let command = format!(
        "{} hook --root {} --home {} --stamp {}{} || exit 2",
        quote(&exe.to_string_lossy()),
        quote(&root.to_string_lossy()),
        quote(&home.root().to_string_lossy()),
        quote(&stamp.to_string_lossy()),
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
    Ok((settings, stamp))
}

/// Managed (administrator) settings load whatever `--setting-sources` says, and two of their
/// keys would silently drop Mestari's hook. Refuse to start agents on such a machine.
fn refuse_hook_policies() -> Result<()> {
    let mut paths = Vec::new();
    for dir in [
        "/etc/claude-code",
        "/Library/Application Support/ClaudeCode",
    ] {
        let dir = Path::new(dir);
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

/// The paja binary the hook runs from: a copy in the factory home, so an agent's own
/// `cargo build` can't replace the binary that judges it. Made once per `paja run` from the
/// binary that is running (on Linux even if the file on disk has been rebuilt since; elsewhere
/// it is the file on disk when the run first needs it); copies other runs
/// left behind are removed once they are a day old.
fn hook_binary(home: &Home) -> Result<PathBuf> {
    static COPIED: std::sync::Mutex<Option<(PathBuf, PathBuf)>> = std::sync::Mutex::new(None);
    let mut copied = COPIED.lock().unwrap_or_else(|e| e.into_inner());
    let _lock = home.lock()?;
    // Another run's cleanup may have removed a copy idle for a day; then copy again.
    if let Some((_, exe)) = copied
        .as_ref()
        .filter(|(root, exe)| root == home.root() && exe.exists())
    {
        // Keep this run's copy fresh so another run's cleanup leaves it alone. Opened read-only:
        // the copy may be running, and a running binary can't be opened for writing.
        if let Ok(file) = std::fs::File::open(exe) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        return Ok(exe.clone());
    }
    let dir = home.root().join("hook");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
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
    // On Linux /proc/self/exe still reads the running binary after a rebuild replaced it.
    let running = match Path::new("/proc/self/exe") {
        p if p.exists() => p.to_path_buf(),
        _ => std::env::current_exe().context("finding the paja binary for the deny-list hook")?,
    };
    // One copy per build of paja, shared by every run of it, so copies don't pile up.
    let meta = std::fs::metadata(&running)
        .with_context(|| format!("reading {} for the deny-list hook", running.display()))?;
    let built = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let exe = dir.join(format!("paja-{}-{built}", meta.len()));
    if !exe.exists() {
        // Copy under a temporary name and rename, so no run ever sees half a binary.
        let partial = dir.join(format!(
            "partial-{}-{}",
            std::process::id(),
            crate::events::now_ms()
        ));
        std::fs::copy(&running, &partial)
            .and_then(|_| std::fs::rename(&partial, &exe))
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })
            .with_context(|| {
                format!(
                    "copying {} to {} for the deny-list hook; if paja was rebuilt, restart `paja \
                     run`",
                    running.display(),
                    exe.display()
                )
            })?;
    }
    // A copy keeps the source's age on some systems; make it new so cleanup leaves it be.
    if let Ok(file) = std::fs::File::open(&exe) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
    *copied = Some((home.root().to_path_buf(), exe.clone()));
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

/// Run each check in the environment. Returns the first failure's command and output. The
/// checks run the branch's code, so they get no more credentials than the agent that wrote it.
fn run_checks(dir: &Path, checks: &[String]) -> Result<Option<String>> {
    let home_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let toolchain = (
        std::env::var_os("CARGO_HOME").map_or(home_dir.join(".cargo"), PathBuf::from),
        std::env::var_os("RUSTUP_HOME").map_or(home_dir.join(".rustup"), PathBuf::from),
    );
    let scratch = tempfile_dir(dir)?;
    let result = (|| {
        for check in checks {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", check]).current_dir(dir);
            without_credentials(&mut cmd, &scratch, git_identity(dir));
            // The checks don't need the owner's home either, and it holds credential files
            // (`.netrc`, cargo's `credentials.toml`) that git and cargo read on their own.
            // Cargo and rustup keep their real directories.
            cmd.env("HOME", &scratch)
                .env("CARGO_HOME", &toolchain.0)
                .env("RUSTUP_HOME", &toolchain.1);
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
