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
            checks.push(claude_version(&claude, text(&out)));
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
            "install the newer release, or rebuild Paja (`cargo install --path crates/paja`), \
             and fix what the detail names",
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

/// Where the latest Claude Code release is looked up: an `http(s)` URL (fetched with `curl`) or a
/// file path, which holds the answer for tests and offline setups.
pub const LATEST_VAR: &str = "PAJA_CLAUDE_LATEST";
const LATEST_URL: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";

/// The dotted number at the start of the first word that has one, e.g. `2.0.0` in
/// `2.0.0 (Claude Code)`.
fn parse_version(text: &str) -> Option<Vec<u64>> {
    text.split(|c: char| c.is_whitespace() || c == '"' || c == 'v')
        .find_map(|word| {
            let parts: Option<Vec<u64>> = word.split('.').map(|p| p.parse().ok()).collect();
            parts.filter(|p| p.len() >= 2)
        })
}

/// Whether version `a` comes before `b`, a missing part counting as 0, so `2.0` and `2.0.0` are
/// the same release.
fn older(a: &[u64], b: &[u64]) -> bool {
    let len = a.len().max(b.len());
    let padded = |v: &[u64]| {
        (0..len)
            .map(|i| v.get(i).copied().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    padded(a) < padded(b)
}

/// The latest released version, or why it couldn't be found. Never waits more than a few seconds.
fn latest_version() -> std::result::Result<Vec<u64>, String> {
    let source = std::env::var(LATEST_VAR).unwrap_or_else(|_| LATEST_URL.to_string());
    let body = if source.starts_with("http://") || source.starts_with("https://") {
        let out = run(Command::new("curl").args([
            "-fsS",
            "--connect-timeout",
            "3",
            "--max-time",
            "5",
            &source,
        ]))?;
        if !out.status.success() {
            return Err(text(&out));
        }
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        std::fs::read_to_string(&source).map_err(|e| format!("reading {source}: {e}"))?
    };
    // The registry answers JSON with a `version`; a plain version number works too.
    let version = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v["version"].as_str().map(str::to_string))
        .unwrap_or(body);
    parse_version(&version).ok_or_else(|| format!("no version in the answer: {}", version.trim()))
}

/// The Claude Code check: it runs, and is it current? Agents get Anthropic's safety updates only
/// through Claude Code updates, so an old one is a warning. A lookup that fails stays ok.
fn claude_version(claude: &str, installed_text: String) -> Check {
    let Some(installed) = parse_version(&installed_text) else {
        return ok("claude", installed_text);
    };
    match latest_version() {
        Ok(latest) if older(&installed, &latest) => {
            let latest = latest
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(".");
            warning(
                "claude",
                format!("{installed_text}; the latest release is {latest}"),
                format!(
                    "run `{claude} update`; agents get Anthropic's safety updates only through \
                     Claude Code updates"
                ),
            )
        }
        Ok(_) => ok("claude", format!("{installed_text} (up to date)")),
        Err(why) => ok(
            "claude",
            format!("{installed_text} (could not check for a newer release: {why})"),
        ),
    }
}

/// `path` as `rev` has it: `None` when the file isn't there (see [`ahjo::show_at`]).
fn show(repo: &Path, rev: &str, path: &str) -> std::result::Result<Option<String>, String> {
    ahjo::show_at(repo, rev, path).map_err(|e| format!("{e:#}"))
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

    // What `paja run` reads, from the default branch: the same refs, in the same order, as
    // `ahjo::show_on_default_branch`, so a default branch with another name is found too.
    let Some(rev) = ["origin/HEAD", "origin/main", "main"]
        .into_iter()
        .find(|rev| {
            git(&["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")])
                .is_ok_and(|out| out.status.success())
        })
    else {
        checks.push(failed(
            "default branch",
            "none of origin/HEAD, origin/main or main exists in this checkout",
            "push the default branch (`git push -u origin main`), then run \
             `git remote set-head origin -a` so Paja knows its name",
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
    // The model per role: a role this Paja doesn't know is skipped on every pass, which only
    // stderr would otherwise say.
    let models_file = crate::mestari::MODELS_FILE;
    match show(proof.path(), rev, models_file) {
        Ok(None) => checks.push(ok(
            "models",
            format!("no {models_file} on {rev}: every role runs on the account's default model"),
        )),
        Ok(Some(text)) => match crate::mestari::Models::parse_on(&text, rev) {
            Err(err) => checks.push(failed(
                "models",
                format!("{err:#}"),
                format!("fix {models_file} on {rev}"),
            )),
            Ok(models) if !models.skipped.is_empty() => checks.push(warning(
                "models",
                format!(
                    "{models_file} on {rev} names role(s) this Paja doesn't know, so their lines \
                     are skipped and the roles run on the default model or turn cap: {}",
                    models.skipped.join(", ")
                ),
                format!(
                    "if a role is new, install the newer Paja; if it is a typo, fix it in \
                     {models_file} on {rev}"
                ),
            )),
            Ok(models) => checks.push(ok(
                "models",
                format!(
                    "{} role(s) set in {models_file} on {rev}",
                    models.roles.len()
                ),
            )),
        },
        Err(err) => checks.push(failed(
            "models",
            format!("reading {models_file} on {rev}: {err}"),
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
            Ok(out) if out.status.success() => {
                checks.push(ok("sandbox", format!("{var} is {program}")))
            }
            // It started but failed: a broken or wrong `sbx` must not look set up.
            Ok(out) => checks.push(failed(
                "sandbox",
                format!("{var} is {program}, and `{program} version` failed: {}", text(&out)),
                format!("fix or reinstall `sbx` and point {var} at it, or unset {var} to run without a sandbox"),
            )),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_of_different_lengths_compare_by_their_numbers() {
        assert!(!older(&[2, 0], &[2, 0, 0]));
        assert!(!older(&[2, 0, 0], &[2, 0]));
        assert!(older(&[2, 0], &[2, 0, 1]));
        assert!(older(&[1, 9, 9], &[2, 0]));
        assert!(!older(&[2, 1], &[2, 0, 9]));
    }
}
