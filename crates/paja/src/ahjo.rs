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

pub fn create(home: &Home, opts: CreateOptions<'_>) -> Result<Env> {
    validate_name(opts.name)?;
    // Under the lock, so a tamper stop recorded meanwhile is seen before any more git runs.
    let lock = home.lock()?;
    let log = home.events();
    let events = log.read_all()?;
    untampered_in(&events, opts.repo)?;
    let repo = main_repo(opts.repo)?;
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

    // Relative gitdir paths (git 2.48+; older git ignores the setting) make the worktree's `.git`
    // file read the same on the host and in the sandbox, where a Windows host path doesn't exist.
    let add: Vec<&str> = if resume {
        vec![
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            &path_str,
            &branch,
        ]
    } else {
        vec![
            "-c",
            "worktree.useRelativePaths=true",
            "worktree",
            "add",
            "-b",
            &branch,
            &path_str,
            &base_commit,
        ]
    };
    git(&repo, &add).with_context(|| format!("creating worktree for {:?}", opts.name))?;

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
        let _ = git(&env.repo, &["worktree", "remove", "--force", &path_str]);
        if !resume {
            let _ = git(&env.repo, &["branch", "-D", &env.branch]);
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
    if env.repo.exists() {
        untampered_in(&events, &env.repo)?;
    }

    if !env.repo.exists() {
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
    } else if env.path.exists() {
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path);
        let removed = git(&env.repo, &args);
        if removed.is_err() && force && !is_registered_worktree(&env.repo, &env.path) {
            // git no longer knows this directory as a worktree (re-cloned repo, lost
            // metadata); with --force, delete it directly and let git forget the rest.
            std::fs::remove_dir_all(&env.path).with_context(|| format!("deleting {path}"))?;
            git(&env.repo, &["worktree", "prune"]).with_context(|| {
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
    } else {
        // The directory is already gone; let git forget about it too.
        git(&env.repo, &["worktree", "prune"]).with_context(|| {
            format!(
                "pruning worktrees of {} for environment {name:?}",
                env.repo.display()
            )
        })?;
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
    let common = git(
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

/// Refuse with [`Tampered`] when the repository at `path` is locked: the log's last tamper
/// event for it is [`EV_TAMPERED`]. Every command that runs git or gh in a repository calls
/// this first. Returns the repository's key.
pub fn ensure_untampered(home: &Home, path: &Path) -> Result<String> {
    untampered_in(&home.events().read_all()?, path)
}

/// [`ensure_untampered`] on events the caller has already read, so a command that replays the
/// log anyway reads it once.
pub fn untampered_in(events: &[Event], path: &Path) -> Result<String> {
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
    Ok(key)
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
fn main_repo(path: &Path) -> Result<PathBuf> {
    let dirs = git(
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
        let bare = git(path, &["rev-parse", "--is-bare-repository"]).is_ok_and(|b| b == "true");
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
    let list = git(path, &["worktree", "list", "--porcelain"])?;
    list.lines()
        .next()
        .and_then(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .with_context(|| format!("can't find the main checkout for {}", path.display()))
}

/// Whether git still lists `path` as one of `repo`'s worktrees.
fn is_registered_worktree(repo: &Path, path: &Path) -> bool {
    let Ok(list) = git(repo, &["worktree", "list", "--porcelain"]) else {
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

/// Run git in `dir` and return trimmed stdout, or an error carrying git's stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
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
    fn a_worktree_records_a_relative_gitdir_so_the_sandbox_can_resolve_it() {
        let out = git_command().arg("--version").output().unwrap();
        let version = String::from_utf8_lossy(&out.stdout).to_string();
        let nums: Vec<u32> = version
            .split_whitespace()
            .nth(2)
            .unwrap_or("")
            .split('.')
            .map_while(|p| p.parse().ok())
            .collect();
        if nums.len() < 2 || (nums[0], nums[1]) < (2, 48) {
            return; // this git has no relative worktree paths
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "--allow-empty",
                "-qm",
                "x",
            ],
        )
        .unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let env = create(
            &home,
            CreateOptions {
                name: "rel",
                repo: &repo,
                base: None,
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(env.path.join(".git")).unwrap();
        let target = text.trim().strip_prefix("gitdir:").unwrap().trim();
        assert!(
            target.starts_with("..") && !target.contains(':'),
            "the .git file should hold a relative path, got {target:?}"
        );
        // And it still resolves on the host.
        git(&env.path, &["status", "--short"]).unwrap();
    }

    #[test]
    fn only_the_last_tamper_event_for_the_same_repository_decides() {
        let dir = tempfile::tempdir().unwrap();
        let repo = |name: &str| {
            let path = dir.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            git(&path, &["init", "-q"]).unwrap();
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
        let tampered =
            |e: anyhow::Result<String>| e.is_err_and(|err| err.chain().any(|c| c.is::<Tampered>()));
        let mut events = vec![event(EV_TAMPERED, &key_a)];
        assert!(tampered(untampered_in(&events, &a)));
        // Another repository's events never lock this one, nor clear it.
        assert_eq!(untampered_in(&events, &b).unwrap(), key_b);
        events.push(event(EV_TAMPER_CHECKED, &key_b));
        assert!(tampered(untampered_in(&events, &a)));
        // Checked, then tampered again: the last one for the key decides each time.
        events.push(event(EV_TAMPER_CHECKED, &key_a));
        assert_eq!(untampered_in(&events, &a).unwrap(), key_a);
        events.push(event(EV_TAMPERED, &key_a));
        assert!(tampered(untampered_in(&events, &a)));
        events.push(event(EV_TAMPERED, &key_b));
        events.push(event(EV_TAMPER_CHECKED, &key_a));
        assert_eq!(untampered_in(&events, &a).unwrap(), key_a);
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
}
