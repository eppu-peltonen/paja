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
    validate_name(opts.name)?;
    if is_paja_name(opts.name) {
        bail!(
            "environment name {:?} has the shape Paja gives its own environments \
             (`<repo>-<hash>-work-<n>`, `<repo>-<hash>-triage`, `work-<n>`, `plan-<n>`, where \
             `<hash>` is any six hex digits), and Paja would reuse, reset and remove it; pick \
             another name",
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
    remove_unless(home, name, force, |_, _| Ok(None))
}

/// [`remove`], unless `keep` names a reason to keep the environment, which is then the error.
/// `keep` runs under the home lock, after the tamper check and before git touches anything, with
/// the log as it stands and the environment, so what it decides can't change before the removal.
pub(crate) fn remove_unless(
    home: &Home,
    name: &str,
    force: bool,
    keep: impl FnOnce(&[Event], &Env) -> Result<Option<String>>,
) -> Result<Env> {
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
    if let Some(why) = keep(&events, &env)? {
        bail!("{why}");
    }

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

/// The top of the checkout `ok` proved safe (it may have been named by a folder inside it).
pub(crate) fn top_level(ok: &Untampered) -> Result<PathBuf> {
    git(ok, ok.path(), &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
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
        // The graft file above is turned off on purpose: no hint about it in error messages.
        "-c",
        "advice.graftFileDeprecated=false",
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
/// `rev` is resolved to one commit first, so a branch that moves meanwhile can't mix two
/// trees. Whether that commit has the file is asked of `git ls-tree` (an empty listing), never
/// read from git's message text, which a translated or changed git words differently; the
/// content is then read from the very blob the listing names.
pub(crate) fn show_at(repo: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    let spec = format!("{rev}:{path}");
    // Refused here rather than with `--end-of-options`, which an older git (before 2.30)
    // doesn't know, and a `:`, which would name a path inside the tree and not a commit: every
    // caller passes a ref name or a commit id.
    if rev.starts_with('-') || rev.contains(':') {
        bail!("reading {spec}: {rev:?} is not a revision; pass a branch name or a commit id");
    }
    let commit = format!("{rev}^{{commit}}");
    let resolved = run_git(repo, &["rev-parse", "--verify", &commit])?;
    if !resolved.status.success() {
        return Err(git_failed(
            &spec,
            repo,
            &format!("git rev-parse {commit}"),
            &resolved,
        ));
    }
    let commit = String::from_utf8_lossy(&resolved.stdout).trim().to_string();
    if path.contains('\0') || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
        bail!("reading {spec}: name a file from the top of the tree, such as .paja/checks");
    }
    let mut path = path.to_string();
    // A symbolic link inside the tree (a CLAUDE.md that links to AGENTS.md, say) is followed
    // to the file it names in the same commit, five links deep at most. A link in the middle
    // of the path (a `.paja` that is itself a link) is an error: git lists nothing under it,
    // and the file must not read as missing when a checkout would have it.
    for links in 0..=5 {
        let entry = blob_at(repo, &commit, &path, &spec)?;
        if matches!(entry, Entry::Missing) {
            if let Some(folder) = not_a_folder(repo, &commit, &path, &spec)? {
                bail!(
                    "reading {spec}: {folder}, on the way to {path}, is a symbolic link or a file, \
                     not a folder; commit the folder itself in its place"
                );
            }
        }
        match entry {
            Entry::Missing if links == 0 => return Ok(None),
            // A link to nothing is never an empty settings file.
            Entry::Missing => bail!(
                "reading {spec}: it is a symbolic link to {path}, which that commit doesn't have; \
                 commit the file itself in its place"
            ),
            Entry::File(bytes) => {
                return String::from_utf8(bytes).map(Some).map_err(|_| {
                    anyhow::anyhow!("reading {spec}: it is not UTF-8 text; save it as UTF-8")
                })
            }
            Entry::Link(target) => path = link_target(&path, &target, &spec)?,
        }
    }
    bail!("reading {spec}: it is a symbolic link more than five links deep; commit the file itself in its place")
}

/// The first leading part of `path` (`a` of `a/b/c`, then `a/b`) that the commit has as
/// something other than a folder, if any.
fn not_a_folder(repo: &Path, commit: &str, path: &str, spec: &str) -> Result<Option<String>> {
    let parts: Vec<&str> = path.split('/').collect();
    for end in 1..parts.len() {
        let prefix = parts[..end].join("/");
        let listed = run_git(
            repo,
            &[
                "--literal-pathspecs",
                "ls-tree",
                "--full-tree",
                "-z",
                commit,
                "--",
                &prefix,
            ],
        )?;
        if !listed.status.success() {
            return Err(git_failed(spec, repo, "git ls-tree", &listed));
        }
        let entry = String::from_utf8_lossy(&listed.stdout);
        match entry.split(' ').nth(1) {
            // Not there: nothing below it is either.
            None => return Ok(None),
            Some("tree") => {}
            Some(_) => return Ok(Some(prefix)),
        }
    }
    Ok(None)
}

/// What one path at a commit is.
enum Entry {
    Missing,
    File(Vec<u8>),
    /// A symbolic link, with the path it points to.
    Link(String),
}

/// `path` at `commit` (a commit id), from the top of the tree and with the path taken
/// literally, as `rev:path` names it.
fn blob_at(repo: &Path, commit: &str, path: &str, spec: &str) -> Result<Entry> {
    let listed = run_git(
        repo,
        &[
            "--literal-pathspecs",
            "ls-tree",
            "--full-tree",
            "-z",
            commit,
            "--",
            path,
        ],
    )?;
    if !listed.status.success() {
        return Err(git_failed(spec, repo, "git ls-tree", &listed));
    }
    let entries: Vec<&[u8]> = listed
        .stdout
        .split(|b| *b == 0)
        .filter(|e| !e.is_empty())
        .collect();
    // `<mode> <type> <object>\t<path>`: exactly the one entry asked for.
    let entry = match entries.as_slice() {
        [] => return Ok(Entry::Missing),
        [one] => String::from_utf8_lossy(one).into_owned(),
        _ => bail!("reading {spec}: {path} names a folder, not a file; name one file in it"),
    };
    let (head, listed_path) = entry
        .split_once('\t')
        .with_context(|| format!("reading {spec}: git ls-tree printed {entry:?}"))?;
    if listed_path != path {
        bail!(
            "reading {spec}: git lists it as {listed_path:?}; name the file from the top of the \
             tree, with no `.` or `..` parts and no trailing `/`"
        );
    }
    let fields: Vec<&str> = head.split(' ').collect();
    let [mode, kind, object] = fields.as_slice() else {
        bail!("reading {spec}: git ls-tree printed {entry:?}");
    };
    match *kind {
        "blob" => {}
        "tree" => bail!("reading {spec}: {path} is a folder, not a file; name one file in it"),
        "commit" => bail!(
            "reading {spec}: {path} is a submodule, not a file; commit the file itself in its place"
        ),
        other => bail!("reading {spec}: {path} is a {other}, not a file"),
    }
    let output = run_git(repo, &["cat-file", "blob", object])?;
    if !output.status.success() {
        return Err(git_failed(
            spec,
            repo,
            &format!("git cat-file blob {object}"),
            &output,
        ));
    }
    if *mode == "120000" {
        let target = String::from_utf8(output.stdout).map_err(|_| {
            anyhow::anyhow!("reading {spec}: {path} is a symbolic link to a name that isn't UTF-8")
        })?;
        return Ok(Entry::Link(target));
    }
    Ok(Entry::File(output.stdout))
}

/// The path, from the top of the tree, that a symbolic link at `link` pointing to `target`
/// names: relative to the link's folder, never outside the tree, and written plainly. A target
/// a checkout could read differently from this text (a `\\`, an empty, `.` or trailing `/`
/// part, a `..` after a name) is refused, not guessed at.
fn link_target(link: &str, target: &str, spec: &str) -> Result<String> {
    let refuse = |why: &str| {
        anyhow::anyhow!(
            "reading {spec}: {link} is a symbolic link to {target:?}, {why}; commit the file \
             itself in its place"
        )
    };
    if target.starts_with('/') || target.as_bytes().get(1) == Some(&b':') {
        return Err(refuse("outside the repository"));
    }
    if target.contains(['\\', '\0']) {
        return Err(refuse("which names a file differently on Windows and Unix"));
    }
    let mut parts: Vec<&str> = link.split('/').collect();
    parts.pop();
    let mut named = false;
    for part in target.split('/') {
        match part {
            "" | "." => return Err(refuse("which isn't a plain path to a file")),
            ".." if named => return Err(refuse("which isn't a plain path to a file")),
            ".." => {
                parts
                    .pop()
                    .ok_or_else(|| refuse("outside the repository"))?;
            }
            name => {
                named = true;
                parts.push(name);
            }
        }
    }
    if !named {
        return Err(refuse("a folder, not a file"));
    }
    Ok(parts.join("/"))
}

/// Run git in `repo`.
fn run_git(repo: &Path, args: &[&str]) -> Result<std::process::Output> {
    git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("running git (is it installed?)")
}

/// The error for a git command that failed while reading `spec`.
fn git_failed(spec: &str, repo: &Path, what: &str, output: &std::process::Output) -> anyhow::Error {
    anyhow::anyhow!(
        "reading {spec}: {what} failed in {}: {}; check that the revision exists there (`git \
         fetch`) and that the repository isn't damaged (`git fsck`)",
        repo.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

/// The commit the ref named exactly `full` (`refs/remotes/origin/main`, say) points at, or None
/// when there is no such ref. Exactly: `git rev-parse` would fall back to a tag or branch named
/// `refs/remotes/origin/main` (`refs/tags/refs/remotes/origin/main`) when the ref is missing.
pub(crate) fn ref_commit(repo: &Path, full: &str) -> Result<Option<String>> {
    let found = run_git(repo, &["show-ref", "--verify", "--hash", full])?;
    if !found.status.success() {
        return Ok(None);
    }
    let object = String::from_utf8_lossy(&found.stdout).trim().to_string();
    let commit = run_git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("{object}^{{commit}}"),
        ],
    )?;
    Ok(commit
        .status
        .success()
        .then(|| String::from_utf8_lossy(&commit.stdout).trim().to_string()))
}

/// The refs that name the default branch, in the order Paja tries them, each with the short name
/// messages use.
pub(crate) const DEFAULT_REFS: [(&str, &str); 3] = [
    ("refs/remotes/origin/HEAD", "origin/HEAD"),
    ("refs/remotes/origin/main", "origin/main"),
    ("refs/heads/main", "main"),
];

/// The prefix of the branches Paja itself creates and pushes (`paja/<name>`), the change under
/// review among them.
const BRANCH_PREFIX: &str = "paja/";

/// The default branch of the checkout `ok` proved safe, as its commit and the short name
/// messages use, or None when no ref names it. Never the branch checked out, which may be the
/// change under review.
///
/// GitHub's name for it decides when `gh repo view` names a usable branch within 15 seconds:
/// Paja reads `origin/<name>` and nothing else, and errors when that isn't fetched. Code in a
/// worktree can move `origin/HEAD` (`git remote set-head`, `git update-ref`), which would point
/// inside `refs/remotes/origin/` once the branch under review is pushed, and can make a local
/// `main`. Otherwise `origin/HEAD` (the remote's default branch, whatever its name), else
/// `origin/main`, else a local `main`; `origin/HEAD` must then be a symbolic ref to a
/// remote-tracking branch that isn't one of Paja's own (`paja/...`), or this errors.
pub(crate) fn default_branch(ok: &Untampered) -> Result<Option<(String, String)>> {
    if let Some(name) = github_default_branch(ok) {
        // GitHub's branch, or an error: a local `main` or `origin/main` may be something else,
        // and code in a worktree can make one.
        return match ref_commit(ok.path(), &format!("refs/remotes/origin/{name}"))? {
            Some(commit) => Ok(Some((commit, format!("origin/{name}")))),
            None => bail!(
                "GitHub's default branch is {name}, but {} has no origin/{name}; run `git fetch \
                 origin {name}` there",
                ok.path().display()
            ),
        };
    }
    trusted_origin_head(ok)?;
    // Full ref names: a short one such as `origin/main` would let a tag or branch of that name
    // (`refs/tags/origin/main`) stand in for the default branch.
    for (full, rev) in DEFAULT_REFS {
        if let Some(commit) = ref_commit(ok.path(), full)? {
            return Ok(Some((commit, rev.to_string())));
        }
    }
    Ok(None)
}

/// Errors when `origin/HEAD` exists but can't be trusted to name the default branch: when it
/// isn't a symbolic ref (one written with `git update-ref`), or points outside
/// `refs/remotes/origin/` or at one of Paja's own branches.
pub(crate) fn trusted_origin_head(ok: &Untampered) -> Result<()> {
    let git = |args: &[&str]| {
        git_command()
            .arg("-C")
            .arg(ok.path())
            .args(args)
            .output()
            .context("running git (is it installed?)")
    };
    let fix = "fix it with `git remote set-head origin -a`";
    let head = git(&["symbolic-ref", "-q", "refs/remotes/origin/HEAD"])?;
    if !head.status.success() {
        // Not symbolic: refused if it exists at all.
        let exists = git(&["show-ref", "--verify", "-q", "refs/remotes/origin/HEAD"])?;
        if exists.status.success() {
            bail!(
                "origin/HEAD in {} is not a symbolic ref, so it is not trusted as the default \
                 branch; {fix}",
                ok.path().display()
            );
        }
        return Ok(());
    }
    let target = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let Some(branch) = target.strip_prefix("refs/remotes/origin/") else {
        bail!(
            "origin/HEAD in {} points at {target}, outside refs/remotes/origin/, so it is not \
             trusted as the default branch; {fix}",
            ok.path().display()
        );
    };
    if branch.starts_with(BRANCH_PREFIX) {
        bail!(
            "origin/HEAD in {} points at {target}, one of Paja's own branches, so it is not \
             trusted as the default branch; {fix}",
            ok.path().display()
        );
    }
    Ok(())
}

/// A file as it is on the default branch (see [`default_branch`]) of the checkout `ok` proved
/// safe. None when the file isn't there, or when the checkout has no `origin` remote and no
/// `main`.
///
/// Errors when `origin/HEAD` can't be trusted, and when the checkout has an `origin` remote but
/// no default branch, so a default branch with another name isn't silently ignored.
pub fn show_on_default_branch(ok: &Untampered, path: &str) -> Result<Option<String>> {
    if let Some((commit, rev)) = default_branch(ok)? {
        return show_at(ok.path(), &commit, path)
            .with_context(|| format!("reading {path} from {rev}, the default branch"));
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
            "{} has an origin remote but gh can't name its default branch, and there is no \
             origin/HEAD, origin/main or main; run `git fetch origin` and `git remote set-head \
             origin -a` there",
            ok.path().display()
        );
    }
    Ok(None)
}

/// How long a default branch name from `gh` is reused, so a `work sync`, which reads two
/// files from the default branch, asks once; and how long gh's failure to say is, so a gh
/// that hangs costs one wait, not two, while the next pass asks again.
const NAME_REUSE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
const NO_NAME_REUSE: std::time::Duration = std::time::Duration::from_secs(30);

/// The default branch's name as `gh repo view` reports it, or None when gh can't say within 15
/// seconds or names something that can't be a branch Paja would trust. The answer is reused
/// per repository: a name for a few minutes, a None for half a minute.
fn github_default_branch(ok: &Untampered) -> Option<String> {
    // Unit tests run without `gh`, whatever the machine has (`GH_REPO`, a signed-in `gh`).
    if cfg!(test) {
        return None;
    }
    type Names = BTreeMap<String, (std::time::Instant, Option<String>)>;
    static NAMES: std::sync::OnceLock<std::sync::Mutex<Names>> = std::sync::OnceLock::new();
    let names = NAMES.get_or_init(Default::default);
    // A panic elsewhere while the lock was held leaves the map as it was, which is still right.
    let lock = || {
        names
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };
    if let Some((at, name)) = lock().get(&ok.key) {
        let reuse = if name.is_some() {
            NAME_REUSE
        } else {
            NO_NAME_REUSE
        };
        if at.elapsed() < reuse {
            return name.clone();
        }
    }
    let name = crate::jono::gh_within(
        ok,
        &["repo", "view", "--json", "defaultBranchRef"],
        Some(std::time::Duration::from_secs(15)),
    )
    .ok()
    .and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok())
    .and_then(|v| {
        v["defaultBranchRef"]["name"]
            .as_str()
            .map(|n| n.trim().to_string())
    })
    .filter(|name| branch_like(name));
    lock().insert(ok.key.clone(), (std::time::Instant::now(), name.clone()));
    name
}

/// Whether `name` could be a branch's name (git's rules, roughly) that isn't one of Paja's own.
fn branch_like(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        // Any letter case: on Windows and macOS `head` opens the same ref file as `HEAD`.
        && !name.eq_ignore_ascii_case("HEAD")
        && !name.starts_with(BRANCH_PREFIX)
        && !name.starts_with(['-', '/', '.'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("/.")
        && !name.contains(".lock/")
        && !name.contains("@{")
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
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
        // A path is taken literally and from the top, wherever git runs.
        assert_eq!(show_at(repo, "HEAD", "*.txt").unwrap(), None);
        std::fs::create_dir_all(repo.join("sub")).unwrap();
        assert_eq!(
            show_at(&repo.join("sub"), "HEAD", "kept.txt")
                .unwrap()
                .as_deref(),
            Some("kept\n")
        );
        let err = show_at(repo, "no-such-rev", "kept.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reading no-such-rev:kept.txt"), "{err}");
        // Nested files, as every real caller reads them.
        std::fs::create_dir_all(repo.join(".paja")).unwrap();
        std::fs::write(repo.join(".paja/checks"), "cargo test\n").unwrap();
        git_unchecked(repo, &["add", ".paja/checks"]).unwrap();
        git_unchecked(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                "d",
            ],
        )
        .unwrap();
        assert_eq!(
            show_at(repo, "HEAD", ".paja/checks").unwrap().as_deref(),
            Some("cargo test\n")
        );
        assert_eq!(show_at(repo, "HEAD", ".paja/models").unwrap(), None);
        // A directory or a spelling git folds is an error, never None and never a listing.
        assert!(show_at(repo, "HEAD", ".paja").is_err());
        assert!(show_at(repo, "HEAD", ".paja/./checks").is_err());
        // A rev that looks like an option, or names a path in a tree, is refused; a full
        // commit id, as the default branch passes, is read.
        for rev in ["--output=x", "HEAD:sub"] {
            let err = show_at(repo, rev, "kept.txt").unwrap_err().to_string();
            assert!(err.contains("is not a revision"), "{err}");
        }
        let head = git_unchecked(repo, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(
            show_at(repo, &head, "kept.txt").unwrap().as_deref(),
            Some("kept\n")
        );
        // An empty file is a file; a symbolic link and a file that isn't UTF-8 are errors. They
        // go in the index by hand, so the test needs no symbolic link on disk (Windows).
        let add = |mode: &str, name: &str, content: &[u8]| {
            let file = dir.path().join("content");
            std::fs::write(&file, content).unwrap();
            let oid = git_unchecked(
                repo,
                &["hash-object", "--no-filters", "-w", &file.to_string_lossy()],
            )
            .unwrap();
            git_unchecked(
                repo,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("{mode},{oid},{name}"),
                ],
            )
            .unwrap();
        };
        add("100644", "empty", b"");
        add("100644", "bom", b"\xef\xbb\xbfa\r\n");
        add("120000", "link", b"kept.txt");
        add("120000", ".paja/link", b"../link");
        add("120000", "outside", b"../kept.txt");
        add("120000", "loop", b"loop");
        add("120000", "dangling", b"gone.txt");
        add("120000", "absolute", b"/etc/passwd");
        add("120000", "drive", b"C:/x");
        add("120000", "backslash", b"d\\x");
        add("120000", "dot", b".");
        add("120000", "climb", b"a/../kept.txt");
        // A chain of five links to a file is followed; a sixth is one too many.
        add("120000", "l1", b"l2");
        add("120000", "l2", b"l3");
        add("120000", "l3", b"l4");
        add("120000", "l4", b"l5");
        add("120000", "l5", b"kept.txt");
        add("120000", "l0", b"l1");
        // A folder that is a link, read through or linked through.
        add("120000", "via", b".paja");
        add("120000", "through", b"via/checks");
        add("100644", "latin1", b"caf\xe9\n");
        git_unchecked(
            repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-qm",
                "e",
            ],
        )
        .unwrap();
        assert_eq!(show_at(repo, "HEAD", "empty").unwrap().as_deref(), Some(""));
        // Byte for byte: a BOM and CRLF line endings stay.
        assert_eq!(
            show_at(repo, "HEAD", "bom").unwrap().as_deref(),
            Some("\u{feff}a\r\n")
        );
        // A symbolic link in the tree is followed, through another link too; one that leaves
        // the tree, or loops, is an error that says what to do.
        assert_eq!(
            show_at(repo, "HEAD", "link").unwrap().as_deref(),
            Some("kept\n")
        );
        assert_eq!(
            show_at(repo, "HEAD", ".paja/link").unwrap().as_deref(),
            Some("kept\n")
        );
        assert_eq!(
            show_at(repo, "HEAD", "l1").unwrap().as_deref(),
            Some("kept\n")
        );
        for name in ["via/checks", "through"] {
            let err = show_at(repo, "HEAD", name).unwrap_err().to_string();
            assert!(err.contains("via, on the way"), "{err}");
        }
        // A folder that isn't there at all still means the file is missing.
        assert_eq!(show_at(repo, "HEAD", "nowhere/checks").unwrap(), None);
        for path in [
            "",
            "/kept.txt",
            "kept.txt/",
            "nowhere/../gone",
            "./kept.txt",
        ] {
            let err = show_at(repo, "HEAD", path).unwrap_err().to_string();
            assert!(err.contains("name a file from the top"), "{err}");
        }
        for name in [
            "outside",
            "loop",
            "dangling",
            "absolute",
            "drive",
            "backslash",
            "dot",
            "climb",
            "l0",
        ] {
            let err = show_at(repo, "HEAD", name).unwrap_err().to_string();
            assert!(err.contains("commit the file itself"), "{err}");
        }
        let err = show_at(repo, "HEAD", "latin1").unwrap_err().to_string();
        assert!(err.contains("UTF-8"), "{err}");
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
        // A tag named like the default branch doesn't stand in for it.
        std::fs::write(repo.join("other"), "").unwrap();
        git_unchecked(repo, &["add", "-A"]).unwrap();
        let mut args = commit.to_vec();
        args.extend(["commit", "-qm", "other"]);
        git_unchecked(repo, &args).unwrap();
        git_unchecked(repo, &["rm", "-q", "--cached", ".paja/labels"]).unwrap();
        let mut args = commit.to_vec();
        args.extend(["commit", "-qm", "no labels"]);
        git_unchecked(repo, &args).unwrap();
        git_unchecked(repo, &["tag", "origin/HEAD"]).unwrap();
        assert_eq!(
            show_on_default_branch(&ok, ".paja/labels")
                .unwrap()
                .as_deref(),
            Some("work x\n")
        );
        git_unchecked(repo, &["tag", "-d", "origin/HEAD"]).unwrap();
        git_unchecked(
            repo,
            &["checkout", "-q", "refs/remotes/origin/trunk", "--", "."],
        )
        .unwrap();
        // Nor does a tag named like the full ref when the ref itself is missing.
        git_unchecked(
            repo,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        )
        .unwrap();
        git_unchecked(repo, &["tag", "refs/remotes/origin/HEAD"]).unwrap();
        assert_eq!(
            ref_commit(repo, "refs/remotes/origin/HEAD").unwrap(),
            None,
            "a tag stood in for origin/HEAD"
        );
        git_unchecked(repo, &["tag", "-d", "refs/remotes/origin/HEAD"]).unwrap();
        git_unchecked(
            repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        )
        .unwrap();
        // An error reading it names the default branch it came from.
        std::fs::remove_file(repo.join(".paja/labels")).unwrap();
        std::fs::create_dir_all(repo.join(".paja/labels")).unwrap();
        std::fs::write(repo.join(".paja/labels/x"), "").unwrap();
        git_unchecked(repo, &["add", "-A"]).unwrap();
        let mut args = commit.to_vec();
        args.extend(["commit", "-qm", "d"]);
        git_unchecked(repo, &args).unwrap();
        git_unchecked(repo, &["update-ref", "refs/remotes/origin/trunk", "HEAD"]).unwrap();
        let err = format!(
            "{:#}",
            show_on_default_branch(&ok, ".paja/labels").unwrap_err()
        );
        assert!(
            err.contains("from origin/HEAD, the default branch"),
            "{err}"
        );
        // origin/HEAD pointing at one of Paja's own branches, such as a pushed review branch,
        // is refused.
        git_unchecked(
            repo,
            &["update-ref", "refs/remotes/origin/paja/app-1", "HEAD"],
        )
        .unwrap();
        git_unchecked(
            repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/paja/app-1",
            ],
        )
        .unwrap();
        let err = show_on_default_branch(&ok, ".paja/labels")
            .unwrap_err()
            .to_string();
        assert!(err.contains("one of Paja's own branches"), "{err}");
        // So is an origin/HEAD that isn't a symbolic ref.
        git_unchecked(
            repo,
            &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
        )
        .unwrap();
        git_unchecked(
            repo,
            &[
                "update-ref",
                "--no-deref",
                "refs/remotes/origin/HEAD",
                "HEAD",
            ],
        )
        .unwrap();
        let err = show_on_default_branch(&ok, ".paja/labels")
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not a symbolic ref"), "{err}");
        assert!(err.contains("git remote set-head origin -a"), "{err}");
    }

    #[test]
    fn a_default_branch_name_from_gh_must_look_like_a_branch() {
        for good in ["main", "trunk", "release/1.0", "dev-x"] {
            assert!(branch_like(good), "{good}");
        }
        for bad in [
            "",
            "@",
            "HEAD",
            "head",
            "paja/app-1",
            "-x",
            "/x",
            ".x",
            "x/",
            "x.",
            "x.lock",
            "a..b",
            "a//b",
            "a/.b",
            "a.lock/b",
            "a@{1}",
            "a b",
            "a\tb",
            "a~1",
            "a^",
            "a:b",
            "a?",
            "a*",
            "a[",
            "a\\b",
        ] {
            assert!(!branch_like(bad), "{bad:?}");
        }
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
