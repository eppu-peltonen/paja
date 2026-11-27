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
    /// Where the errors and their fixes are explained.
    pub help: &'static str,
}

/// The troubleshooting guide in Paja's public repository, which works wherever `paja` runs.
pub const TROUBLESHOOTING_URL: &str =
    "https://github.com/eppu-peltonen/paja/blob/main/docs/troubleshooting.md";

/// Where Paja's issues are filed.
const ISSUES_URL: &str = "https://github.com/eppu-peltonen/paja/issues";

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
    Ok(Report {
        checks,
        ok,
        help: TROUBLESHOOTING_URL,
    })
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
    // Whether the account has a usage window, from `claude auth status`; assumed until it says.
    let mut windowed = true;
    match run(Command::new(&claude).arg("--version")) {
        Ok(out) if out.status.success() => {
            checks.push(claude_version(&claude, text(&out)));
            match run(Command::new(&claude).args(["auth", "status"])) {
                Ok(out) if out.status.success() => {
                    windowed = has_window(&String::from_utf8_lossy(&out.stdout));
                    checks.push(ok("claude sign-in", "signed in"));
                }
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
    checks.push(left_over_environments(home));
    checks.push(match home.events().read_all() {
        Ok(events) => usage_window(&events, &claude, windowed),
        Err(err) => warning(
            "usage window",
            format!("couldn't read the event log: {err:#}"),
            format!("check {}", home.root().display()),
        ),
    });
}

/// How many of the latest agent steps the usage window check reads.
const WINDOW_STEPS: usize = 5;

/// Whether `claude auth status` (JSON) says the account has a usage window to report: signed
/// in to a Claude subscription, not with an API key or through a cloud provider. Output that
/// isn't the JSON expected counts as a subscription, so the check still runs.
fn has_window(status: &str) -> bool {
    let Ok(status) = serde_json::from_str::<Value>(status) else {
        return true;
    };
    let provider = status["apiProvider"].as_str().unwrap_or("firstParty");
    let method = status["authMethod"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    provider == "firstParty" && !method.contains("api")
}

/// Whether Claude Code's results tell Paja the usage window. The window comes from the
/// `rate_limit_event` messages of `claude -p --output-format stream-json`, whose shape Paja
/// assumes; if a Claude Code prints it differently, no step records a window and a nearly used
/// one never holds a build back, silently. So the latest agent steps that ran a session are
/// read: none at all is ok (nothing has run yet), and none of them with a window is a warning.
/// `windowed` is false for an account with no window (an API key, a cloud provider).
fn usage_window(events: &[crate::events::Event], claude: &str, windowed: bool) -> Check {
    let name = "usage window";
    if !windowed {
        return ok(
            name,
            "signed in with an API key or through a cloud provider: there is no usage window to \
             report",
        );
    }
    let mut steps: Vec<_> = crate::mestari::StepUsage::recorded(events)
        // A step that ran a session; one that never started has nothing to report.
        .filter(|s| s.session_id.is_some())
        .collect();
    steps.drain(..steps.len().saturating_sub(WINDOW_STEPS));
    let Some(latest) = steps.iter().rfind(|s| s.reports_window()) else {
        if steps.is_empty() {
            return ok(name, "no agent step has run yet");
        }
        return warning(
            name,
            format!(
                "the latest {} agent step{} reported no usage window, so a nearly used window \
                 can't hold a build back before Claude Code's limit cuts it off",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            ),
            format!(
                "run `{claude} update`. If Claude Code is current, file an issue at {ISSUES_URL} \
                 with the `rate_limit_event` line that `{claude} -p --output-format stream-json \
                 --verbose hi` prints: Paja reads the window from its `rate_limit_info`. Billed \
                 per token (an API key, a Console sign-in), there is no window and this warning \
                 doesn't apply"
            ),
        );
    };
    let mut detail = format!(
        "the latest step that reported a window had {}",
        latest.window_status.as_deref().unwrap_or("no status")
    );
    if let Some(used) = latest.window_used_percent {
        detail.push_str(&format!(", {used:.0}% used"));
    }
    ok(name, detail)
}

/// Environments whose work item's issue was closed: a parked item keeps its environment (so the
/// builder's edits survive the question), and nothing removes it if the issue is closed instead
/// of answered.
fn left_over_environments(home: &Home) -> Check {
    let name = "environments";
    let found = (|| -> Result<Vec<String>> {
        let events = home.events().read_all()?;
        let envs = ahjo::replay(&events)?;
        let mut left: Vec<String> = jono::replay(&events, "")?
            .values()
            .filter(|item| !item.open && item.status != jono::Status::Active)
            .map(|item| item.env_name())
            .filter(|env| envs.contains_key(env))
            .collect();
        left.sort();
        left.dedup();
        Ok(left)
    })();
    match found {
        Ok(left) if left.is_empty() => ok(name, "none is left over from a closed item"),
        Ok(left) => warning(
            name,
            format!(
                "{} left over from closed items, still holding worktrees and ports: {}",
                if left.len() == 1 {
                    "1 environment is"
                } else {
                    "environments are"
                },
                left.join(", ")
            ),
            format!(
                "if the work isn't needed, run {}; it keeps the branch, and refuses while the \
                 worktree has uncommitted changes (add --force to discard them)",
                left.iter()
                    .map(|env| format!("`paja env remove {env}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Err(err) => warning(
            name,
            format!("couldn't read the event log: {err:#}"),
            format!("check {}", home.root().display()),
        ),
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
    // Resolved once, by exact ref name: every file below is read from that one commit, and
    // messages name the branch by its short name.
    let mut found = None;
    for (full, short) in crate::ahjo::DEFAULT_REFS {
        match crate::ahjo::ref_commit(proof.path(), full) {
            Ok(Some(commit)) => {
                found = Some((commit, short));
                break;
            }
            Ok(None) => {}
            Err(err) => {
                checks.push(failed(
                    "default branch",
                    format!("looking up {short}: {err:#}"),
                    "run `git fsck` in this checkout and check that `git` runs",
                ));
                return;
            }
        }
    }
    let Some((commit, rev)) = found else {
        checks.push(failed(
            "default branch",
            "none of origin/HEAD, origin/main or main exists in this checkout",
            "push the default branch (`git push -u origin main`), then run \
             `git remote set-head origin -a` so Paja knows its name",
        ));
        return;
    };
    // The work label.
    let labels = match show(proof.path(), &commit, jono::LABELS_FILE) {
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
    match show(proof.path(), &commit, crate::mestari::CHECKS_FILE) {
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
    match show(proof.path(), &commit, models_file) {
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
                    // Quoted and escaped as the run's own warning shows them, so a control
                    // character in the file never reaches the terminal.
                    models
                        .skipped
                        .iter()
                        .map(|role| format!("{role:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
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
    let mode = show(proof.path(), &commit, ci_file).map(|t| lines(&t.unwrap_or_default()));
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

    fn rounds(steps: &[serde_json::Value]) -> Vec<crate::events::Event> {
        steps
            .iter()
            .enumerate()
            .map(|(i, step)| crate::events::Event {
                v: 1,
                seq: i as u64 + 1,
                ts_ms: 0,
                kind: crate::mestari::EV_ROUND.to_string(),
                data: serde_json::json!({ "steps": [step] }),
            })
            .collect()
    }

    #[test]
    fn the_usage_window_check_reads_the_latest_five_steps_that_ran() {
        let windowed = serde_json::json!({"step": "build", "session_id": "s",
            "window_status": "allowed", "window_used_percent": 40.0});
        let bare = serde_json::json!({"step": "build", "session_id": "s"});
        let unstarted = serde_json::json!({"step": "build"});
        let check = |steps: &[serde_json::Value]| usage_window(&rounds(steps), "claude", true);
        assert_eq!(check(&[]).status, Status::Ok);
        assert_eq!(check(std::slice::from_ref(&unstarted)).status, Status::Ok);
        // A window four steps back is among the latest five.
        let mut steps = vec![windowed.clone()];
        steps.extend(std::iter::repeat_n(bare.clone(), 4));
        let ok = check(&steps);
        assert_eq!(ok.status, Status::Ok);
        assert!(ok.detail.contains("had allowed, 40% used"), "{}", ok.detail);
        // Steps that never ran a session don't push it out.
        steps.extend(std::iter::repeat_n(unstarted, 3));
        assert_eq!(check(&steps).status, Status::Ok);
        // Five steps since it, none with a window: a warning that names the configured claude.
        steps.push(bare);
        let warned = usage_window(&rounds(&steps), "my-claude", true);
        assert_eq!(warned.status, Status::Warning);
        assert!(
            warned.detail.contains("latest 5 agent steps"),
            "{}",
            warned.detail
        );
        let fix = warned.fix.unwrap();
        assert!(
            fix.contains("`my-claude update`") && fix.contains(ISSUES_URL),
            "{fix}"
        );
        // An account with no window is never warned.
        assert_eq!(
            usage_window(&rounds(&steps), "claude", false).status,
            Status::Ok
        );
    }

    #[test]
    fn only_a_subscription_has_a_usage_window() {
        assert!(has_window(
            r#"{"authMethod": "oauth_token", "apiProvider": "firstParty"}"#
        ));
        assert!(has_window(r#"{"loggedIn": true}"#));
        assert!(has_window("Logged in"));
        assert!(!has_window(
            r#"{"authMethod": "api_key", "apiProvider": "firstParty"}"#
        ));
        assert!(!has_window(
            r#"{"authMethod": "oauth_token", "apiProvider": "bedrock"}"#
        ));
    }

    #[test]
    fn versions_of_different_lengths_compare_by_their_numbers() {
        assert!(!older(&[2, 0], &[2, 0, 0]));
        assert!(!older(&[2, 0, 0], &[2, 0]));
        assert!(older(&[2, 0], &[2, 0, 1]));
        assert!(older(&[1, 9, 9], &[2, 0]));
        assert!(!older(&[2, 1], &[2, 0, 9]));
    }
}
