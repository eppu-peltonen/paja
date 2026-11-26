//! `paja doctor`: check that this machine and repository are ready for `paja run`, before it
//! fails partway. Changes nothing in the repository; every failure comes with its fix.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::ahjo;
use crate::home::Home;
use crate::jono;

/// How one check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Paja runs, but the person should know.
    Warning,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warning => "warning",
            Status::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// What to do about a warning or failure; null when the check is ok.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    /// No check failed (warnings don't count).
    pub ok: bool,
}

fn ok(name: &str, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: Status::Ok,
        detail: detail.into(),
        fix: None,
    }
}

fn failed(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        status: Status::Failed,
        detail: detail.into(),
        fix: Some(fix.into()),
    }
}

fn warning(name: &str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check {
        status: Status::Warning,
        ..failed(name, detail, fix)
    }
}

/// Run `cmd` with no stdin and no prompts; `Err` is why it couldn't start.
fn run(cmd: &mut Command) -> std::result::Result<Output, String> {
    crate::platform::own_process_group(cmd)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| err.to_string())
}

fn text(output: &Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let text = text.trim();
    // A long error would bury the other checks.
    text.lines().take(4).collect::<Vec<_>>().join(" ")
}

/// Non-empty lines that are not `#` comments.
fn lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// Run every check for the repository at `repo`.
pub fn run_checks(home: &Home, repo: &Path) -> Result<Report> {
    let repo = std::path::absolute(repo)?;
    let mut checks = Vec::new();
    machine(home, &repo, &mut checks);
    repository(home, &repo, &mut checks);
    sandbox(&mut checks);
    let ok = checks.iter().all(|c| c.status != Status::Failed);
    Ok(Report { checks, ok })
}

fn machine(home: &Home, repo: &Path, checks: &mut Vec<Check>) {
    // git
    match run(ahjo::git_command().arg("--version")) {
        Ok(out) if out.status.success() => checks.push(ok("git", text(&out))),
        _ => checks.push(failed(
            "git",
            "`git` did not run",
            "install Git (https://git-scm.com/downloads) and open a new terminal",
        )),
    }
    if cfg!(windows) {
        match crate::platform::posix_shell() {
            Ok(sh) => checks.push(ok("sh", sh.display().to_string())),
            Err(err) => checks.push(failed(
                "sh",
                format!("{err:#}"),
                "install Git for Windows (https://git-scm.com/downloads/win), or set \
                 CLAUDE_CODE_GIT_BASH_PATH to its bash.exe",
            )),
        }
    }
    // gh
    let gh = jono::gh_program();
    match run(Command::new(&gh).arg("--version")) {
        Ok(out) if out.status.success() => {
            checks.push(ok(
                "gh",
                text(&out).lines().next().unwrap_or("gh").to_string(),
            ));
            match run(Command::new(&gh).args(["auth", "status"])) {
                Ok(out) if out.status.success() => checks.push(ok("gh sign-in", "signed in")),
                Ok(out) => checks.push(failed(
                    "gh sign-in",
                    format!("`{gh} auth status` failed: {}", text(&out)),
                    format!("run `{gh} auth login`"),
                )),
                Err(err) => checks.push(failed(
                    "gh sign-in",
                    format!("running `{gh} auth status`: {err}"),
                    format!("run `{gh} auth login`"),
                )),
            }
        }
        _ => checks.push(failed(
            "gh",
            format!("`{gh}` did not run"),
            "install the GitHub CLI (https://cli.github.com), then run `gh auth login`",
        )),
    }
    // claude
    let claude = std::env::var("PAJA_CLAUDE").unwrap_or_else(|_| "claude".to_string());
    match run(Command::new(&claude).arg("--version")) {
        Ok(out) if out.status.success() => {
            checks.push(ok("claude", text(&out)));
            match run(Command::new(&claude).args(["auth", "status"])) {
                Ok(out) if out.status.success() => checks.push(ok("claude sign-in", "signed in")),
                Ok(out) => checks.push(failed(
                    "claude sign-in",
                    format!("`{claude} auth status` failed: {}", text(&out)),
                    format!("run `{claude}` once and sign in, then exit"),
                )),
                Err(err) => checks.push(failed(
                    "claude sign-in",
                    format!("running `{claude} auth status`: {err}"),
                    format!("run `{claude}` once and sign in, then exit"),
                )),
            }
        }
        _ => checks.push(failed(
            "claude",
            format!("`{claude}` did not run"),
            "install Claude Code (https://claude.ai/install), then run `claude` once to sign in",
        )),
    }
    // The hook and the home folder.
    match crate::mestari::check_hook(home, repo) {
        Ok(()) => checks.push(ok(
            "deny-list hook",
            "blocks `git push` and allows `git status`",
        )),
        Err(err) => checks.push(failed(
            "deny-list hook",
            format!("{err:#}"),
            "rebuild and reinstall Paja (`cargo install --path crates/paja`), and fix what the \
             detail names",
        )),
    }
    let dir = home.root();
    match crate::platform::check_private_dir(dir) {
        Ok(()) => checks.push(ok(
            "home folder",
            format!("{} is private to this user", dir.display()),
        )),
        Err(err) => {
            let why = err.to_string();
            // The Windows message ends with advice for the hook's folder, not for the home.
            let why = why
                .split_once(", so another account")
                .map_or(why.clone(), |(a, _)| {
                    format!("{a}, so another account could change what's in it")
                });
            let fix = if cfg!(windows) {
                // The owner first: an elevated account's folders belong to Administrators.
                format!(
                    "run `icacls \"{0}\" /setowner \"%USERNAME%\"` and then `icacls \"{0}\" \
                     /inheritance:r /grant:r \"%USERNAME%:(OI)(CI)F\"`",
                    dir.display()
                )
            } else {
                format!("run `chmod 700 \"{}\"`", dir.display())
            };
            checks.push(failed("home folder", why, fix));
        }
    }
}

/// `path` as `rev` has it: `None` when the file isn't there.
fn show(repo: &Path, rev: &str, path: &str) -> std::result::Result<Option<String>, String> {
    let out = run(ahjo::git_command()
        .arg("-C")
        .arg(repo)
        .args(["show", &format!("{rev}:{path}")]))?;
    if out.status.success() {
        return Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()));
    }
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    if err.contains("does not exist in") || err.contains("exists on disk, but not in") {
        return Ok(None);
    }
    Err(err.trim().to_string())
}

fn repository(home: &Home, repo: &Path, checks: &mut Vec<Check>) {
    let proof = match ahjo::untampered(home, repo) {
        Ok(proof) => proof,
        Err(err) => {
            checks.push(failed(
                "repository",
                format!("{err:#}"),
                "run `paja doctor` inside the repository's checkout, or pass --repo",
            ));
            return;
        }
    };
    let git = |args: &[&str]| run(ahjo::git_command().arg("-C").arg(proof.path()).args(args));
    // origin
    match git(&["remote", "get-url", "origin"]) {
        Ok(out) if out.status.success() => {
            let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if url.contains("github.com") {
                checks.push(ok("origin", url));
            } else {
                checks.push(warning(
                    "origin",
                    format!("{url} is not on github.com"),
                    "Paja lands through GitHub pull requests; this is fine only for a GitHub \
                     Enterprise host that `gh` is signed in to",
                ));
            }
        }
        _ => checks.push(failed(
            "origin",
            "this repository has no `origin` remote",
            "add the GitHub repository: `git remote add origin https://github.com/<owner>/<repo>.git`",
        )),
    }
    // gh's view of the repository.
    let gh = jono::gh_program();
    let gh_in_repo = |args: &[&str]| {
        run(Command::new(&gh).current_dir(proof.path()).args(args))
            .and_then(|out| {
                if out.status.success() {
                    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
                } else {
                    Err(text(&out))
                }
            })
            .map_err(|err| format!("`{gh} {}` failed: {err}", args.join(" ")))
    };
    let access = "gh access";
    match gh_in_repo(&["repo", "view", "--json", "viewerPermission"]) {
        Ok(out) => match serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v["viewerPermission"].as_str().map(str::to_string))
        {
            Some(p) if ["ADMIN", "MAINTAIN", "WRITE"].contains(&p.as_str()) => {
                checks.push(ok(access, format!("your permission is {p}")))
            }
            Some(p) => checks.push(failed(
                access,
                format!("your permission is {p}; Paja pushes branches and merges pull requests"),
                "ask the repository's owner for write access, or sign in to another account with \
                 `gh auth login`",
            )),
            None => checks.push(failed(
                access,
                format!("`{gh} repo view` gave no viewerPermission: {out}"),
                "update the GitHub CLI",
            )),
        },
        Err(err) => checks.push(failed(
            access,
            err,
            "check `gh auth status` and that `origin` is a GitHub repository you can reach",
        )),
    }

    // What `paja run` reads, from the default branch.
    let Some(rev) = ["origin/main", "main"].into_iter().find(|rev| {
        git(&["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
            .is_ok_and(|out| out.status.success())
    }) else {
        checks.push(failed(
            "default branch",
            "neither origin/main nor main exists in this checkout",
            "commit on `main` and push it: `git push -u origin main`",
        ));
        return;
    };
    // The work label.
    let labels = match show(proof.path(), rev, jono::LABELS_FILE) {
        Ok(text) => jono::Labels::parse(&text.unwrap_or_default()).map_err(|e| format!("{e:#}")),
        Err(err) => Err(err),
    };
    match labels {
        Err(err) => checks.push(failed(
            "work label",
            err,
            format!("fix {} on the default branch", jono::LABELS_FILE),
        )),
        Ok(labels) => {
            let label = labels.work;
            let found =
                gh_in_repo(&["label", "list", "--json", "name", "--limit", "1000"]).map(|out| {
                    serde_json::from_str::<Value>(&out).ok().is_some_and(|v| {
                        v.as_array().is_some_and(|all| {
                            all.iter()
                                .any(|l| l["name"].as_str() == Some(label.as_str()))
                        })
                    })
                });
            match found {
                Ok(true) => checks.push(ok("work label", format!("`{label}` exists"))),
                Ok(false) => checks.push(failed(
                    "work label",
                    format!("the repository has no `{label}` label"),
                    format!("run `{gh} label create {label}`"),
                )),
                Err(err) => checks.push(failed(
                    "work label",
                    err,
                    "check `gh auth status` and that `origin` is a GitHub repository you can reach",
                )),
            }
        }
    }
    // Vasara's checks.
    match show(proof.path(), rev, crate::mestari::CHECKS_FILE) {
        Ok(Some(t)) if !lines(&t).is_empty() => checks.push(ok(
            "checks",
            format!(
                "{} command(s) in {} on {rev}",
                lines(&t).len(),
                crate::mestari::CHECKS_FILE
            ),
        )),
        Ok(Some(_)) => checks.push(failed(
            "checks",
            format!("{} on {rev} has no commands", crate::mestari::CHECKS_FILE),
            "put one check command per line in it (for example `cargo test`), commit and push",
        )),
        Ok(None) => checks.push(failed(
            "checks",
            format!("{} is missing on {rev}", crate::mestari::CHECKS_FILE),
            "add it with one check command per line (for example `cargo test`), commit and push \
             it to the default branch",
        )),
        Err(err) => checks.push(failed(
            "checks",
            format!("reading {} on {rev}: {err}", crate::mestari::CHECKS_FILE),
            "check that the repository is intact (`git status`)",
        )),
    }
    // CI.
    let ci_file = crate::mestari::CI_FILE;
    let mode = show(proof.path(), rev, ci_file).map(|t| lines(&t.unwrap_or_default()));
    let required = || match gh_in_repo(&["workflow", "list"]) {
        Ok(list) if !list.is_empty() => ok("ci", "a GitHub Actions workflow is set up"),
        Ok(_) => failed(
            "ci",
            "the repository has no GitHub Actions workflow, so Paja would wait for CI that \
             never comes",
            format!(
                "add a workflow that runs the checks on pull requests, or put `none` in \
                 {ci_file} on the default branch"
            ),
        ),
        Err(err) => failed(
            "ci",
            err,
            "check `gh auth status` and that `origin` is a GitHub repository you can reach",
        ),
    };
    checks.push(match mode {
        Err(err) => failed(
            "ci",
            format!("reading {ci_file} on {rev}: {err}"),
            "check that the repository is intact (`git status`)",
        ),
        Ok(words) => match words.as_slice() {
            [w] if w == "none" => ok("ci", format!("{ci_file} says none")),
            [] => required(),
            [w] if w == "required" => required(),
            _ => failed(
                "ci",
                format!("{ci_file} on {rev} must hold `required` or `none`"),
                format!("fix {ci_file} on the default branch"),
            ),
        },
    });
}

fn sandbox(checks: &mut Vec<Check>) {
    let var = crate::sandbox::PROGRAM_VAR;
    match crate::sandbox::configured() {
        Some(program) => match run(Command::new(&program).arg("version")) {
            Ok(_) => checks.push(ok("sandbox", format!("{var} is {program}"))),
            Err(err) => checks.push(failed(
                "sandbox",
                format!("{var} is {program}, which did not run: {err}"),
                format!("install `sbx` and point {var} at it, or unset {var} to run without a sandbox"),
            )),
        },
        None => checks.push(warning(
            "sandbox",
            format!("{var} is not set, so agents and checks run as this user, with your files and sign-ins"),
            format!("install `sbx` and set {var} to it for a sandbox (see docs/setup.md)"),
        )),
    }
}
