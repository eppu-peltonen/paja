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
    /// Commit the branch was created from.
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
    let repo = main_repo(opts.repo)?;
    let branch = format!("paja/{}", opts.name);
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
    let start = if resume {
        branch.as_str()
    } else {
        opts.base.unwrap_or("HEAD")
    };
    let base_commit = git(
        &repo,
        &["rev-parse", "--verify", &format!("{start}^{{commit}}")],
    )
    .with_context(|| format!("{start:?} is not a commit in {}", repo.display()))?;

    let lock = home.lock()?;
    let log = home.events();
    let envs = replay(&log.read_all()?)?;
    if envs.contains_key(opts.name) {
        bail!("an environment named {:?} already exists", opts.name);
    }
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

/// Remove an environment's worktree. The branch is kept, since it may hold work that has not
/// landed yet. Without `force`, a worktree with uncommitted changes is refused.
pub fn remove(home: &Home, name: &str, force: bool) -> Result<Env> {
    let lock = home.lock()?;
    let log = home.events();
    let env = replay(&log.read_all()?)?.remove(name).with_context(|| {
        format!("no environment named {name:?}; run `paja env list` to see them")
    })?;
    let path = env
        .path
        .to_str()
        .with_context(|| format!("environment path {} is not UTF-8", env.path.display()))?;

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
        if removed.is_err() && force {
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
                format!("removing worktree {path}")
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
    .with_context(|| {
        format!(
            "{} is not inside a git checkout with a working tree",
            path.display()
        )
    })?;
    let mut lines = dirs.lines();
    let (git_dir, common_dir, top) = (lines.next(), lines.next(), lines.next());
    let top = top.with_context(|| {
        format!(
            "{} is in a bare repository; run paja from a checkout",
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

/// Run git in `dir` and return trimmed stdout, or an error carrying git's stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
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
