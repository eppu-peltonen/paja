//! Ahjo: isolated environments. Each one is a git worktree on its own branch with its own
//! block of ports, so many agents can work on the same repo at once without colliding.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::events::{now_ms, Event};
use crate::home::Home;

/// First port handed out. Chosen above common dev-server ports and below the ephemeral range.
pub const PORT_START: u16 = 20_000;
/// Ports per environment.
pub const PORT_BLOCK: u16 = 100;
/// Most environments that can exist at once (keeps every block below 30000).
pub const MAX_ENVS: u16 = 100;

pub const EV_CREATED: &str = "env.created";
pub const EV_REMOVED: &str = "env.removed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Env {
    pub name: String,
    /// Top level of the repository the worktree belongs to.
    pub repo: PathBuf,
    /// The worktree directory agents work in.
    pub path: PathBuf,
    /// Branch checked out in the worktree.
    pub branch: String,
    /// Commit the environment started from: the base of a new branch, or the tip of a
    /// resumed one.
    pub base_commit: String,
    /// First port of this environment's block; it owns `port_base..port_base + port_count`.
    pub port_base: u16,
    pub port_count: u16,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Removed {
    name: String,
}

/// Current environments, derived by replaying the event log.
pub fn replay(events: &[Event]) -> Result<BTreeMap<String, Env>> {
    let mut envs = BTreeMap::new();
    for event in events {
        match event.kind.as_str() {
            EV_CREATED => {
                let env: Env = serde_json::from_value(event.data.clone())
                    .with_context(|| format!("event {} has a malformed env", event.seq))?;
                envs.insert(env.name.clone(), env);
            }
            EV_REMOVED => {
                let removed: Removed = serde_json::from_value(event.data.clone())
                    .with_context(|| format!("event {} has a malformed removal", event.seq))?;
                envs.remove(&removed.name);
            }
            _ => {}
        }
    }
    Ok(envs)
}

pub fn list(home: &Home) -> Result<Vec<Env>> {
    Ok(replay(&home.events().read_all()?)?.into_values().collect())
}

pub fn get(home: &Home, name: &str) -> Result<Env> {
    replay(&home.events().read_all()?)?
        .remove(name)
        .with_context(|| format!("no environment named {name:?}; run `paja env list` to see them"))
}

pub struct CreateOptions<'a> {
    pub name: &'a str,
    /// Any path inside the repository; defaults to the current directory in the CLI.
    pub repo: &'a Path,
    /// Commit-ish to branch from; `HEAD` of the repo when `None`.
    pub base: Option<&'a str>,
}

/// [`create`] for a person (`paja env create`, the MCP tool): names shaped like the ones Paja
/// gives its own environments are refused, since Paja would reuse, reset and remove them.
pub fn create_by_person(home: &Home, opts: CreateOptions<'_>) -> Result<Env> {
    if is_paja_name(opts.name) {
        bail!(
            "environment name {:?} has the shape Paja gives its own environments \
             (`<repo>-<hash>-work-<n>`, `<repo>-<hash>-triage`, `work-<n>`, `plan-<n>`), and Paja \
             would reuse, reset and remove it; pick another name",
            opts.name
        );
    }
    create(home, opts)
}

/// Whether `name` has the shape of a name Paja gives its own environments: what
/// [`crate::jono::env_name_for`] makes (an optional `<repo>-`, six hex digits, then `-triage` or
/// `-work-<n>`), a plan's `plan-<n>`, or the `work-<n>` of releases before names carried the
/// repository.
pub fn is_paja_name(name: &str) -> bool {
    let number = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    if ["work-", "plan-"]
        .iter()
        .any(|p| name.strip_prefix(p).is_some_and(number))
    {
        return true;
    }
    let head = if let Some(head) = name.strip_suffix("-triage") {
        head
    } else if let Some((head, n)) = name.rsplit_once("-work-") {
        if !number(n) {
            return false;
        }
        head
    } else {
        return false;
    };
    let hash = head.rsplit('-').next().unwrap_or(head);
    hash.len() == 6 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn create(home: &Home, opts: CreateOptions<'_>) -> Result<Env> {
    validate_name(opts.name)?;
    // Under the lock, so a tamper stop recorded meanwhile is seen before any more git runs.
    let lock = home.lock()?;
    let log = home.events();
    let events = log.read_all()?;
    let ok = untampered_in(&events, opts.repo)?;
    let repo = main_repo(&ok, opts.repo)?;
    let branch = format!("paja/{}", opts.name);

    let envs = replay(&events)?;
    if envs.contains_key(opts.name) {
        bail!(
            "an environment named {:?} already exists; pick another name or run `paja env remove {}` first",
            opts.name,
            opts.name
        );
    }

    // `remove` keeps branches, so a reused name may find its old branch: resume it.
    let resume = git(
        &ok,
        &repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_ok();
    if resume && opts.base.is_some() {
        bail!(
            "branch {branch} already exists (kept from an earlier environment), so --base can't apply; \
             omit --base to resume it, or delete it with `git -C {} branch -D {branch}`",
            repo.display()
        );
    }
    // HEAD and relative names (`HEAD~1`, `@{u}`) mean what they mean where the caller stands,
    // which may be a linked worktree rather than the main checkout.
    let (start, start_in) = if resume {
        (branch.as_str(), repo.as_path())
    } else {
        (opts.base.unwrap_or("HEAD"), opts.repo)
    };
    let base_commit = git(
        &ok,
        start_in,
        &["rev-parse", "--verify", &format!("{start}^{{commit}}")],
    )
    .with_context(|| format!("{start:?} is not a commit in {}", start_in.display()))?;

    let port_base = free_port_block(&envs)?;
    let path = home.envs_dir().join(opts.name);
    if path.exists() {
        bail!(
            "{} already exists but is not a known environment; move it away first",
            path.display()
        );
    }
    std::fs::create_dir_all(home.envs_dir())
        .with_context(|| format!("creating {}", home.envs_dir().display()))?;
    let path_str = path
        .to_str()
        .with_context(|| format!("environment path {} is not UTF-8", path.display()))?
        .to_string();

    let add: Vec<&str> = if resume {
        vec!["worktree", "add", &path_str, &branch]
    } else {
        vec!["worktree", "add", "-b", &branch, &path_str, &base_commit]
    };
    git(&ok, &repo, &add).with_context(|| format!("creating worktree for {:?}", opts.name))?;

    let env = Env {
        name: opts.name.to_string(),
        repo,
        path,
        branch,
        base_commit,
        port_base,
        port_count: PORT_BLOCK,
        created_ms: now_ms(),
    };
    let recorded = serde_json::to_value(&env)
        .map_err(anyhow::Error::from)
        .and_then(|data| log.append(&lock, EV_CREATED, data));
    if let Err(err) = recorded {
        // If the event made it into the log (say, only the sync failed), the environment is
        // real and must be kept; undoing the worktree would leave the log pointing at nothing.
        let landed = log.read_all().ok().is_some_and(|events| {
            replay(&events).is_ok_and(|envs| envs.get(&env.name) == Some(&env))
        });
        if landed {
            return Err(err.context(format!(
                "environment {:?} was recorded, but the log could not be synced to disk",
                opts.name
            )));
        }
        // Without its event the worktree would be an orphan that blocks the name; undo it.
        let _ = git(
            &ok,
            &env.repo,
            &["worktree", "remove", "--force", &path_str],
        );
        if !resume {
            let _ = git(&ok, &env.repo, &["branch", "-D", &env.branch]);
        }
        return Err(err.context(format!(
            "recording environment {:?}; the worktree was rolled back",
            opts.name
        )));
    }
    Ok(env)
}

/// What removing an environment reports: the name, and the branch that was kept.
#[derive(Debug, Clone, Serialize)]
pub struct RemoveReport {
    pub removed: String,
    pub branch_kept: String,
}

impl From<&Env> for RemoveReport {
    fn from(env: &Env) -> Self {
        Self {
            removed: env.name.clone(),
            branch_kept: env.branch.clone(),
        }
    }
}

/// Remove an environment's worktree. The branch is kept, since it may hold work that has not
/// landed yet. Without `force`, a worktree with uncommitted changes is refused.
pub fn remove(home: &Home, name: &str, force: bool) -> Result<Env> {
    let lock = home.lock()?;
    let log = home.events();
    let events = log.read_all()?;
    let env = replay(&events)?.remove(name).with_context(|| {
        format!("no environment named {name:?}; run `paja env list` to see them")
    })?;
    let path = env
        .path
        .to_str()
        .with_context(|| format!("environment path {} is not UTF-8", env.path.display()))?;
    // git runs only where the repository still exists, and only once the tamper lock allows it.
    let checked = if env.repo.exists() {
        Some(untampered_in(&events, &env.repo)?)
    } else {
        None
    };

    match checked {
        None => {
            // git can't run against a repository that is gone; only the directory is left.
            if env.path.exists() {
                if !force {
                    bail!(
                        "repository {} no longer exists, so git can't remove the worktree; \
                     pass --force to delete {path} directly",
                        env.repo.display()
                    );
                }
                std::fs::remove_dir_all(&env.path).with_context(|| format!("deleting {path}"))?;
            }
        }
        Some(ok) if env.path.exists() => {
            let mut args = vec!["worktree", "remove"];
            if force {
                args.push("--force");
            }
            args.push(path);
            let removed = git(&ok, &env.repo, &args);
            if removed.is_err() && force && !is_registered_worktree(&ok, &env.repo, &env.path) {
                // git no longer knows this directory as a worktree (re-cloned repo, lost
                // metadata); with --force, delete it directly and let git forget the rest.
                std::fs::remove_dir_all(&env.path).with_context(|| format!("deleting {path}"))?;
                git(&ok, &env.repo, &["worktree", "prune"]).with_context(|| {
                    format!(
                        "pruning worktrees of {} after deleting {path}",
                        env.repo.display()
                    )
                })?;
            } else {
                removed.with_context(|| {
                if force {
                    // git still tracks it and refuses: say how to clear what git is guarding.
                    format!(
                        "removing worktree {path}; git still tracks it, so paja won't delete it \
                         itself. If it is locked, run `git -C {repo} worktree unlock {path}`; if \
                         git refuses for another reason, run `git -C {repo} worktree remove -f -f {path}`. \
                         Then run `paja env remove {name} --force` again",
                        repo = env.repo.display()
                    )
                } else {
                    format!("removing worktree {path} (if it has uncommitted changes, --force discards them)")
                }
            })?;
            }
        }
        Some(ok) => {
            // The directory is already gone; let git forget about it too.
            git(&ok, &env.repo, &["worktree", "prune"]).with_context(|| {
                format!(
                    "pruning worktrees of {} for environment {name:?}",
                    env.repo.display()
                )
            })?;
        }
    }

    log.append(
        &lock,
        EV_REMOVED,
        serde_json::to_value(Removed { name: name.into() })?,
    )?;
    Ok(env)
}

/// Branch code changed git's configuration in `repo`; Paja runs no git or gh there until a
/// person has checked it and recorded [`EV_TAMPER_CHECKED`].
pub const EV_TAMPERED: &str = "run.tampered";
pub const EV_TAMPER_CHECKED: &str = "run.tamper_checked";

/// The error Paja raises when git's configuration changed under it, or when a command is
/// asked to run git or gh in a repository locked by [`EV_TAMPERED`]. Nothing may run git in
/// that repository afterwards, not even to clean up, until a person has looked.
#[derive(Debug)]
pub struct Tampered(pub(crate) String);

impl std::fmt::Display for Tampered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Tampered {}

/// The key the tamper lock uses for the repository at any path inside it, however that path
/// was spelled: its shared git directory, which rev-parse finds without running any program
/// from the configuration.
pub fn repo_key(path: &Path) -> Result<String> {
    let common = git_unchecked(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .with_context(|| {
        format!(
            "{} is not inside a git checkout; run paja from the repository's checkout, or pass \
             `--repo` (the `repo` argument of the MCP tools) a path inside it",
            path.display()
        )
    })?;
    Ok(std::fs::canonicalize(&common)
        .unwrap_or_else(|_| PathBuf::from(&common))
        .display()
        .to_string())
}

/// Proof that the tamper lock allowed git and gh in a repository. Only [`untampered`] makes
/// one, and Ahjo's and Jono's git and gh helpers take it, so a new command can't run them
/// there without the check.
pub struct Untampered {
    key: String,
    path: PathBuf,
}

impl Untampered {
    /// The repository's key (see [`repo_key`]).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The path the check was made for, where gh runs.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Refuse with [`Tampered`] when the repository at `path` is locked: the log's last tamper
/// event for it is [`EV_TAMPERED`]. Every command that runs git or gh in a repository calls
/// this first and passes the proof to the helper that runs them.
pub fn untampered(home: &Home, path: &Path) -> Result<Untampered> {
    untampered_in(&home.events().read_all()?, path)
}

/// [`untampered`], returning only the repository's key, for Mestari, which runs git and gh
/// through its own helpers.
pub fn ensure_untampered(home: &Home, path: &Path) -> Result<String> {
    untampered(home, path).map(|ok| ok.key)
}

/// [`untampered`] on events the caller has already read, so a command that replays the log
/// anyway reads it once. The caller must have read them under the home lock it still holds,
/// or a tamper stop recorded since would go unseen; so this stays private to Ahjo.
fn untampered_in(events: &[Event], path: &Path) -> Result<Untampered> {
    let key = repo_key(path)?;
    let last = events
        .iter()
        .rev()
        .find(|e| (e.kind == EV_TAMPERED || e.kind == EV_TAMPER_CHECKED) && e.data["repo"] == key);
    if let Some(event) = last.filter(|e| e.kind == EV_TAMPERED) {
        return Err(Tampered(format!(
            "Paja stopped on {key} because git's configuration changed during a run ({}); it \
             runs no git or gh there until a person has checked and restored those files and \
             runs `paja run --tamper-checked`",
            event.data["error"].as_str().unwrap_or("see the event log")
        ))
        .into());
    }
    Ok(Untampered {
        key,
        path: path.to_path_buf(),
    })
}

/// A person has checked the repository at `path` after a tamper stop: record it, so Paja
/// runs git and gh there again. Returns the repository's key.
pub fn clear_tampered(home: &Home, path: &Path) -> Result<String> {
    let key = repo_key(path)?;
    let lock = home.lock()?;
    home.events()
        .append(&lock, EV_TAMPER_CHECKED, serde_json::json!({ "repo": key }))?;
    Ok(key)
}

/// The main repository's top level for any path inside it, even from inside a linked
/// worktree (an agent creating an environment from within its own environment) or a
/// submodule. git lists the main worktree first.
fn main_repo(ok: &Untampered, path: &Path) -> Result<PathBuf> {
    let dirs = git(
        ok,
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
            "--show-toplevel",
        ],
    )
    .map_err(|err| {
        let bare = git(ok, path, &["rev-parse", "--is-bare-repository"]).is_ok_and(|b| b == "true");
        if bare {
            err.context(format!(
                "{} is a bare repository; run paja from a checkout of it",
                path.display()
            ))
        } else {
            err.context(format!(
                "{} is not inside a git checkout with a working tree",
                path.display()
            ))
        }
    })?;
    let mut lines = dirs.lines();
    let (git_dir, common_dir, top) = (lines.next(), lines.next(), lines.next());
    let top = top.with_context(|| {
        format!(
            "{} is a bare repository; run paja from a checkout of it",
            path.display()
        )
    })?;
    if git_dir == common_dir {
        // Already the main checkout (this also covers submodules).
        return Ok(PathBuf::from(top));
    }
    // A linked worktree: git lists the main worktree first.
    let list = git(ok, path, &["worktree", "list", "--porcelain"])?;
    list.lines()
        .next()
        .and_then(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .with_context(|| format!("can't find the main checkout for {}", path.display()))
}

/// Whether `env` was made from the repository `path` is in (any checkout or worktree of it).
/// Environment names hold only a short hash of `owner/name`, so two clones of one repository
/// under one home share them; only git in `path` runs, after `path`'s tamper check.
pub fn belongs_to(home: &Home, env: &Env, path: &Path) -> Result<bool> {
    let ok = untampered(home, path)?;
    let main = main_repo(&ok, path)?;
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    Ok(canonical(&main) == canonical(&env.repo))
}

/// Whether git still lists `path` as one of `repo`'s worktrees.
fn is_registered_worktree(ok: &Untampered, repo: &Path, path: &Path) -> bool {
    let Ok(list) = git(ok, repo, &["worktree", "list", "--porcelain"]) else {
        return true; // can't tell: keep git's own error rather than deleting
    };
    let want = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    list.lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p)) == want)
}

fn free_port_block(envs: &BTreeMap<String, Env>) -> Result<u16> {
    (0..MAX_ENVS)
        .map(|i| PORT_START + i * PORT_BLOCK)
        .find(|base| envs.values().all(|env| env.port_base != *base))
        .with_context(|| format!("all {MAX_ENVS} port blocks are in use; remove an environment"))
}

fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!(
            "environment name {name:?} must be 1-40 characters of a-z, 0-9 and '-', \
             not starting or ending with '-'"
        );
    }
    Ok(())
}

/// Git as Paja runs it, with the owner's credentials. Code run in a worktree could plant hooks
/// or a file-system monitor for it to run, so those are always off.
pub(crate) fn git_command() -> Command {
    let mut cmd = Command::new("git");
    // Ctrl-C on `paja run` stops it between steps; git must not die mid-command meanwhile.
    // Outside the terminal's process group git can't prompt (it would stop, waiting on the
    // terminal), so it fails instead of asking.
    crate::platform::own_process_group(&mut cmd);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // `/dev/null` works on Windows too: Git for Windows maps it to `NUL`.
    // Replacement refs would let code in a worktree change the history Paja judges.
    cmd.env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null");
    cmd.args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
        // Paths come back as plain text, so non-ASCII names still match high-risk entries.
        "-c",
        "core.quotePath=false",
    ]);
    cmd
}

/// A file as it is at `rev` in `repo`, or None if `rev` doesn't have it. Any other git failure
/// is an error: a settings or check file that can't be read must never count as an empty one.
/// Git runs in the C locale, since a missing file is told apart by git's English message.
pub(crate) fn show_at(repo: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{rev}:{path}");
    let output = git_command()
        .env("LC_ALL", "C")
        .env_remove("LANGUAGE")
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

/// A file as it is on the default branch of the checkout `ok` proved safe: `origin/HEAD` (the
/// remote's default branch, whatever its name), else `origin/main`, else a local `main`. None
/// when the file isn't there, or when there is none of these. Never the branch checked out,
/// which may be the change under review.
///
/// Errors when `origin/HEAD` points outside `refs/remotes/origin/` (code in a worktree can
/// point it at the branch under review), and when the checkout has an `origin` remote but
/// none of the three refs, so a default branch with another name isn't silently ignored.
pub fn show_on_default_branch(ok: &Untampered, path: &str) -> Result<Option<String>> {
    let head = git_command()
        .arg("-C")
        .arg(ok.path())
        .args(["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])
        .output()
        .context("running git (is it installed?)")?;
    if head.status.success() {
        let target = String::from_utf8_lossy(&head.stdout).trim().to_string();
        if !target.starts_with("refs/remotes/origin/") {
            bail!(
                "origin/HEAD in {} points at {target}, outside refs/remotes/origin/, so it is not \
                 trusted as the default branch; fix it with `git remote set-head origin -a`",
                ok.path().display()
            );
        }
    }
    let resolves = |rev: &str| -> Result<bool> {
        let spec = format!("{rev}^{{commit}}");
        Ok(git_command()
            .arg("-C")
            .arg(ok.path())
            .args(["rev-parse", "--verify", "-q", &spec])
            .output()
            .context("running git (is it installed?)")?
            .status
            .success())
    };
    for rev in ["origin/HEAD", "origin/main", "main"] {
        if resolves(rev)? {
            return show_at(ok.path(), rev, path);
        }
    }
    // No origin/HEAD and no main: ask GitHub for the default branch's name, as `RepoSettings`
    // does, and read it from the remote-tracking ref.
    if let Some(name) = github_default_branch(ok) {
        let rev = format!("origin/{name}");
        if resolves(&rev)? {
            return show_at(ok.path(), &rev, path);
        }
    }
    let has_origin = git_command()
        .arg("-C")
        .arg(ok.path())
        .args(["remote", "get-url", "origin"])
        .output()
        .context("running git (is it installed?)")?
        .status
        .success();
    if has_origin {
        bail!(
            "{} has an origin remote but none of origin/HEAD, origin/main or main, so the \
             default branch is unknown; run `git remote set-head origin -a` there",
            ok.path().display()
        );
    }
    Ok(None)
}

/// The default branch's name as `gh repo view` reports it, or None when gh can't say.
fn github_default_branch(ok: &Untampered) -> Option<String> {
    let out = crate::jono::gh(ok, &["repo", "view", "--json", "defaultBranchRef"]).ok()?;
    let v: serde_json::Value = serde_json::from_str(&out).ok()?;
    let name = v["defaultBranchRef"]["name"].as_str()?.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Run git in `dir`, a path in the repository the tamper lock allowed (`_ok`), and return
/// trimmed stdout, or an error carrying git's stderr. Taking the proof means no command can
/// run git here without the check.
fn git(_ok: &Untampered, dir: &Path, args: &[&str]) -> Result<String> {
    git_unchecked(dir, args)
}

/// [`git`] without the tamper lock: only for [`repo_key`], whose rev-parse the check itself
/// needs and which runs nothing from the configuration, and for tests.
fn git_unchecked(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str, port_base: u16) -> Env {
        Env {
            name: name.into(),
            repo: "/r".into(),
            path: format!("/e/{name}").into(),
            branch: format!("paja/{name}"),
            base_commit: "abc".into(),
            port_base,
            port_count: PORT_BLOCK,
            created_ms: 0,
        }
    }

    #[test]
    fn show_at_tells_a_missing_file_from_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git_unchecked(repo, &["init", "-q"]).unwrap();
        std::fs::write(repo.join("kept.txt"), "kept\n").unwrap();
        git_unchecked(repo, &["add", "kept.txt"]).unwrap();
        git_unchecked(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                "c",
            ],
        )
        .unwrap();
        // On disk but not committed: still missing at HEAD.
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        assert_eq!(
            show_at(repo, "HEAD", "kept.txt").unwrap().as_deref(),
            Some("kept\n")
        );
        assert_eq!(show_at(repo, "HEAD", "gone.txt").unwrap(), None);
        assert_eq!(show_at(repo, "HEAD", "new.txt").unwrap(), None);
        let err = show_at(repo, "no-such-rev", "kept.txt")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("git show no-such-rev:kept.txt failed in"),
            "{err}"
        );
    }

    #[test]
    fn default_branch_refuses_a_moved_origin_head_and_a_remote_without_one() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git_unchecked(repo, &["init", "-q", "-b", "trunk"]).unwrap();
        std::fs::create_dir_all(repo.join(".paja")).unwrap();
        std::fs::write(repo.join(".paja/labels"), "work x\n").unwrap();
        git_unchecked(repo, &["add", "-A"]).unwrap();
        let commit = ["-c", "user.name=t", "-c", "user.email=t@example.com"];
        let mut args = commit.to_vec();
        args.extend(["commit", "-qm", "c"]);
        git_unchecked(repo, &args).unwrap();
        let ok = Untampered {
            key: "k".into(),
            path: repo.to_path_buf(),
        };
        // No remote: nothing to say about a default branch.
        assert_eq!(show_on_default_branch(&ok, ".paja/labels").unwrap(), None);
        // A remote, but no origin/HEAD and no main: say what to do.
        git_unchecked(
            repo,
            &["remote", "add", "origin", "https://example.com/r.git"],
        )
        .unwrap();
        let err = show_on_default_branch(&ok, ".paja/labels")
            .unwrap_err()
            .to_string();
        assert!(err.contains("git remote set-head origin -a"), "{err}");
        // origin/HEAD pointing at a local branch is refused.
        git_unchecked(
            repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/heads/trunk",
            ],
        )
        .unwrap();
        let err = show_on_default_branch(&ok, ".paja/labels")
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside refs/remotes/origin/"), "{err}");
        // Pointing inside refs/remotes/origin/ is read.
        git_unchecked(repo, &["update-ref", "refs/remotes/origin/trunk", "HEAD"]).unwrap();
        git_unchecked(
            repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        )
        .unwrap();
        assert_eq!(
            show_on_default_branch(&ok, ".paja/labels")
                .unwrap()
                .as_deref(),
            Some("work x\n")
        );
    }

    #[test]
    fn only_the_last_tamper_event_for_the_same_repository_decides() {
        let dir = tempfile::tempdir().unwrap();
        let repo = |name: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            git_unchecked(&path, &["init", "-q"]).unwrap();
            path
        };
        let (a, b) = (repo("a"), repo("b"));
        let (key_a, key_b) = (repo_key(&a).unwrap(), repo_key(&b).unwrap());
        assert_ne!(key_a, key_b);
        let mut seq = 0;
        let mut event = |kind: &str, key: &str| {
            seq += 1;
            Event {
                v: 1,
                seq,
                ts_ms: 0,
                kind: kind.into(),
                data: serde_json::json!({ "repo": key, "error": "config changed" }),
            }
        };
        let tampered = |e: anyhow::Result<Untampered>| {
            e.is_err_and(|err| err.chain().any(|c| c.is::<Tampered>()))
        };
        let mut events = vec![event(EV_TAMPERED, &key_a)];
        assert!(tampered(untampered_in(&events, &a)));
        // Another repository's events never lock this one, nor clear it.
        assert_eq!(untampered_in(&events, &b).unwrap().key, key_b);
        events.push(event(EV_TAMPER_CHECKED, &key_b));
        assert!(tampered(untampered_in(&events, &a)));
        // Checked, then tampered again: the last one for the key decides each time.
        events.push(event(EV_TAMPER_CHECKED, &key_a));
        assert_eq!(untampered_in(&events, &a).unwrap().key, key_a);
        events.push(event(EV_TAMPERED, &key_a));
        assert!(tampered(untampered_in(&events, &a)));
        events.push(event(EV_TAMPERED, &key_b));
        events.push(event(EV_TAMPER_CHECKED, &key_a));
        assert_eq!(untampered_in(&events, &a).unwrap().key, key_a);
        assert!(tampered(untampered_in(&events, &b)));
    }

    #[test]
    fn port_blocks_reuse_the_lowest_free_slot() {
        let mut envs = BTreeMap::new();
        assert_eq!(free_port_block(&envs).unwrap(), 20_000);
        envs.insert("a".into(), env("a", 20_000));
        envs.insert("c".into(), env("c", 20_200));
        assert_eq!(free_port_block(&envs).unwrap(), 20_100);
    }

    #[test]
    fn names_are_validated() {
        for good in ["a", "fix-login", "chunk-42"] {
            validate_name(good).unwrap();
        }
        for bad in [
            "",
            "-a",
            "a-",
            "Upper",
            "has space",
            "slash/no",
            &"x".repeat(41),
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn paja_shaped_names_are_told_apart_from_a_persons() {
        for repo in ["acme/app", "acme/a-very-long-repository-name", "acme/---"] {
            for what in ["triage", "work-7", "work-12345"] {
                let name = crate::jono::env_name_for(repo, what);
                assert!(is_paja_name(&name), "{name}");
            }
        }
        for name in ["work-7", "plan-12"] {
            assert!(is_paja_name(name), "{name}");
        }
        for theirs in [
            "triage",
            "work-",
            "plan-a",
            "work-7-fix",
            "fix-triage",
            "app-12345-triage",
            "app-abcdeg-triage",
            "app-abcdef-work-",
            "app-abcdef-work-7a",
            "app-abcdef-worker",
        ] {
            assert!(!is_paja_name(theirs), "{theirs}");
        }
    }
}
