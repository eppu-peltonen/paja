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
        .with_context(|| format!("no environment named {name:?}"))
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
    let repo = PathBuf::from(git(opts.repo, &["rev-parse", "--show-toplevel"])?);
    let base = opts.base.unwrap_or("HEAD");
    let base_commit = git(
        &repo,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )
    .with_context(|| format!("{base:?} is not a commit in {}", repo.display()))?;
    let branch = format!("paja/{}", opts.name);

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
    std::fs::create_dir_all(home.envs_dir())?;

    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path.to_str().context("environment path is not UTF-8")?,
            &base_commit,
        ],
    )
    .with_context(|| format!("creating worktree for {:?}", opts.name))?;

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
    log.append(&lock, EV_CREATED, serde_json::to_value(&env)?)?;
    Ok(env)
}

/// Remove an environment's worktree. The branch is kept, since it may hold work that has not
/// landed yet. Without `force`, a worktree with uncommitted changes is refused.
pub fn remove(home: &Home, name: &str, force: bool) -> Result<Env> {
    let lock = home.lock()?;
    let log = home.events();
    let env = replay(&log.read_all()?)?
        .remove(name)
        .with_context(|| format!("no environment named {name:?}"))?;

    if env.path.exists() {
        let path = env.path.to_str().context("environment path is not UTF-8")?;
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path);
        git(&env.repo, &args).with_context(|| {
            if force {
                format!("removing worktree {path}")
            } else {
                format!("removing worktree {path} (use --force to discard uncommitted changes)")
            }
        })?;
    } else {
        // The directory is already gone; let git forget about it too.
        git(&env.repo, &["worktree", "prune"])?;
    }

    log.append(
        &lock,
        EV_REMOVED,
        serde_json::to_value(Removed { name: name.into() })?,
    )?;
    Ok(env)
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
