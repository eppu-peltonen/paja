//! The optional sandbox: one long-lived Docker Sandboxes (`sbx`) sandbox that Mestari runs agent
//! sessions and Vasara's checks in, so the code they start can't reach the owner's files or
//! credentials. See "The sandbox" in `docs/design/architecture.md`.
//!
//! The backend is optional: it is on when `PAJA_SBX` names the `sbx` program, and then it fails
//! closed. Nothing here needs Docker unless it is on, so tests use a fake `sbx`.
//!
//! Everything Paja assumes about `sbx` is in this file: `sbx exec [-w DIR] [-e K=V] NAME -- CMD`,
//! `sbx create --name NAME AGENT PATH...`, and that a mounted path has the same absolute path
//! inside the sandbox.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// The environment variable that names the `sbx` program and turns the sandbox on.
pub const PROGRAM_VAR: &str = "PAJA_SBX";

/// The sandbox's name. One per machine; it is reused by every run.
const NAME: &str = "paja";

/// Where code in the sandbox keeps its throwaway `PAJA_HOME`: the sandbox's own `/tmp`, never a
/// path on the host.
pub const SCRATCH: &str = "/tmp/paja-home";

/// Puts rustup's directory on `PATH`, then runs the command it is given.
const WRAP: &str = r#"PATH="$HOME/.cargo/bin:$PATH"; export PATH; exec "$@""#;

/// A shell function that runs a command (an `apt-get`), retrying while apt's lists or dpkg lock
/// is held. A fresh sandbox runs its own `apt-get` just after it starts. It tries
/// `$PAJA_APT_TRIES` times (default 60) `$PAJA_APT_INTERVAL` seconds apart (default 5), about
/// five minutes, then fails with apt's message, which names the lock. Tests override both.
const APT_RETRY: &str = r#"apt_retry() {
  tries=0
  max="${PAJA_APT_TRIES:-60}"
  while :; do
    if out=$("$@" 2>&1); then
      printf '%s\n' "$out"
      return 0
    fi
    case "$out" in
      *"Could not get lock"*|*"Unable to lock"*)
        tries=$((tries + 1))
        if [ "$tries" -ge "$max" ]; then
          printf '%s\n' "$out" >&2
          echo "apt's lock is still held after $tries tries (the lock is named above); wait for the other apt-get or dpkg to finish and run again" >&2
          return 1
        fi
        sleep "${PAJA_APT_INTERVAL:-5}"
        ;;
      *)
        printf '%s\n' "$out" >&2
        return 1
        ;;
    esac
  done
}"#;

/// Installs a C toolchain when the sandbox lacks one. Runs as root (`sbx exec -u root`): the
/// agent user has no working `sudo` in the sandbox.
fn setup_root() -> String {
    format!(
        "set -e\n{APT_RETRY}\n\
         if ! command -v cc >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then\n  \
         apt_retry apt-get update\n  \
         apt_retry apt-get install -y build-essential curl ca-certificates git\nfi"
    )
}

/// Installs Rust (what Vasara's checks need) for the agent user, when the sandbox lacks it.
const SETUP_USER: &str = r#"set -e
if [ ! -x "$HOME/.cargo/bin/cargo" ] && ! command -v cargo >/dev/null 2>&1; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c rustfmt,clippy
fi"#;

/// Whether the toolchain is already there.
const TOOLCHAIN_CHECK: &str = "cc --version && cargo --version && rustfmt --version";

#[derive(Clone, Debug)]
pub struct Sbx {
    program: String,
    name: String,
}

static ACTIVE: Mutex<Option<Sbx>> = Mutex::new(None);

/// Use `sandbox` for every agent and check from now on, or none.
pub fn activate(sandbox: Option<Sbx>) {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = sandbox;
}

/// The sandbox in use, if the run started one.
pub fn active() -> Option<Sbx> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The `sbx` program, when the person turned the sandbox on.
pub fn configured() -> Option<String> {
    std::env::var(PROGRAM_VAR)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
}

/// Start (or reuse) the sandbox with `mounts` reachable inside it. `Ok(None)` when it isn't
/// configured; an error, never a silent fallback, when it is configured but can't start.
pub fn start(mounts: &[PathBuf]) -> Result<Option<Sbx>> {
    let Some(program) = configured() else {
        return Ok(None);
    };
    let sbx = Sbx {
        program,
        name: NAME.to_string(),
    };
    if sbx.exec_ok("true").is_err() {
        let mut create = Command::new(&sbx.program);
        create
            .args(["create", "--name", &sbx.name, "claude"])
            .args(mounts);
        let out = create
            .stdin(Stdio::null())
            .output()
            .with_context(|| sbx.unavailable())?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("already exists") {
                bail!(
                    "the sandbox `{name}` already exists but its mounts don't match this run (a \
                     removed factory home or directory?): {}; remove it with `{} rm {name}` and \
                     run again",
                    stderr.trim(),
                    sbx.program,
                    name = sbx.name
                );
            }
            bail!(
                "creating the sandbox with `{} create --name {} claude ...` failed: {}; {}",
                sbx.program,
                sbx.name,
                String::from_utf8_lossy(&out.stderr).trim(),
                sbx.fix()
            );
        }
        sbx.exec_ok("true").with_context(|| {
            format!(
                "the sandbox `{}` was created but won't run commands; {}",
                sbx.name,
                sbx.fix()
            )
        })?;
    }
    // A sandbox that was already there may have been made for another factory home or another
    // set of mounts; agents would then fail later on a raw `-w` error.
    for mount in mounts {
        if sbx.exec_in(mount, "true").is_err() {
            bail!(
                "the sandbox `{name}` exists but `{}` is not mounted in it (it was made for \
                 another factory home or repository); remove it with `{} rm {name}` and run \
                 again, and Paja creates it with the right mounts",
                mount.display(),
                sbx.program,
                name = sbx.name
            );
        }
    }
    if sbx.exec_ok(TOOLCHAIN_CHECK).is_err() {
        sbx.exec_as(Some("root"), &setup_root())
            .and_then(|()| sbx.exec_as(None, SETUP_USER))
            .with_context(|| {
                format!(
                    "setting up Rust and a C toolchain in the sandbox `{}` failed; run \
                 `{} exec -it {} sh` and install `cc`, `cargo` and `rustfmt` there",
                    sbx.name, sbx.program, sbx.name
                )
            })?;
        sbx.exec_ok(TOOLCHAIN_CHECK).with_context(|| {
            format!(
                "the sandbox `{}` still has no Rust or C toolchain after setup; run \
                 `{} exec -it {} sh` and install `cc`, `cargo` and `rustfmt` there",
                sbx.name, sbx.program, sbx.name
            )
        })?;
    }
    Ok(Some(sbx))
}

/// `path` as the sandbox spells it. The sandbox is Linux: on Windows `C:\a\b` is `/c/a/b` there
/// (drive letter lower-cased, backslashes to slashes); elsewhere the path is unchanged. A UNC
/// path has no such spelling and is refused. Use this for every host path handed to `sbx` for
/// use inside the sandbox (`exec -w`, command lines, variables), but not for `sbx create`
/// mounts, which `sbx` reads on the host.
pub fn in_sandbox_path(path: &Path) -> Result<String> {
    translate(&path.to_string_lossy())
}

/// `GIT_DIR` and `GIT_WORK_TREE` for a linked worktree at `dir`, spelled for the sandbox. A
/// worktree's `.git` file holds the host's gitdir path (`C:/.../.git/worktrees/x`), which doesn't
/// exist in the sandbox on Windows. Empty when `dir` is not a linked worktree, when the paths need
/// no translation (not Windows), or when `env` already sets either variable.
fn git_env(dir: &Path, env: &[(String, String)]) -> Vec<(String, String)> {
    if !cfg!(windows)
        || env
            .iter()
            .any(|(k, _)| k == "GIT_DIR" || k == "GIT_WORK_TREE")
    {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(dir.join(".git")) else {
        return Vec::new();
    };
    let Some(gitdir) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")) else {
        return Vec::new();
    };
    match (translate(gitdir.trim()), in_sandbox_path(dir)) {
        (Ok(git_dir), Ok(work_tree)) => vec![
            ("GIT_DIR".to_string(), git_dir),
            ("GIT_WORK_TREE".to_string(), work_tree),
        ],
        _ => Vec::new(),
    }
}

fn translate(path: &str) -> Result<String> {
    if !cfg!(windows) {
        return Ok(path.to_string());
    }
    // `\\?\C:\a` is the verbatim form of a drive path; `\\?\UNC\..` and `\\server\..` are UNC.
    let plain = path.strip_prefix(r"\\?\").unwrap_or(path);
    let b = plain.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let rest = &plain[2..];
        if rest.is_empty() || rest.starts_with(['\\', '/']) {
            let rest = rest.replace('\\', "/");
            return Ok(format!("/{}{}", (b[0] as char).to_ascii_lowercase(), rest));
        }
        bail!("`{path}` is a drive-relative path; give the full path, like `C:\\dir`");
    }
    if plain.starts_with(r"\\") || plain.starts_with("//") || path.starts_with(r"\\?\") {
        bail!(
            "`{path}` is a network (UNC) path, which the sandbox can't mount; use a directory on \
             a local drive, such as `C:\\...`, for the factory home and the repository"
        );
    }
    // Already a Linux-style path (`/work`, `/`).
    Ok(path.replace('\\', "/"))
}

impl Sbx {
    fn unavailable(&self) -> String {
        format!(
            "running `{}` ({} names it): {}",
            self.program,
            PROGRAM_VAR,
            self.fix()
        )
    }

    fn fix(&self) -> String {
        format!(
            "install Docker Sandboxes (`sbx`) and sign in to Docker, or unset {} to run agents \
             behind the deny-list hook only",
            PROGRAM_VAR
        )
    }

    /// The command that runs `argv` in the sandbox, in `dir`, with exactly `env` set. The
    /// sandbox does not inherit the host's environment, so nothing else reaches it.
    pub fn command(
        &self,
        dir: &Path,
        env: &[(String, String)],
        argv: &[String],
    ) -> Result<Command> {
        let mut cmd = Command::new(&self.program);
        cmd.arg("exec").arg("-w").arg(in_sandbox_path(dir)?);
        for (key, value) in env {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        for (key, value) in git_env(dir, env) {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        cmd.arg(&self.name)
            .args(["--", "sh", "-c", WRAP, "sh"])
            .args(argv);
        Ok(cmd)
    }

    /// The command that runs the shell script `script` (a check) in the sandbox.
    pub fn script(&self, dir: &Path, env: &[(String, String)], script: &str) -> Result<Command> {
        self.command(
            dir,
            env,
            &["sh".to_string(), "-c".to_string(), script.to_string()],
        )
    }

    /// Whether Claude Code's transcript of session `id` is in the sandbox's home, which a resume
    /// needs. Only a plain id (letters, digits, `-`) is looked for; anything else says no.
    pub fn has_transcript(&self, id: &str) -> bool {
        id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && self
                .exec_ok(&format!(
                    "ls \"$HOME\"/.claude/projects/*/{id}.jsonl >/dev/null 2>&1"
                ))
                .is_ok()
    }

    fn exec_ok(&self, script: &str) -> Result<()> {
        self.exec_as(None, script)
    }

    /// Run `script` in the sandbox, in `dir`.
    fn exec_in(&self, dir: &Path, script: &str) -> Result<()> {
        self.exec_run(Some(dir), None, script)
    }

    /// Run `script` in the sandbox as `user` (the sandbox's default user when `None`).
    fn exec_as(&self, user: Option<&str>, script: &str) -> Result<()> {
        self.exec_run(None, user, script)
    }

    fn exec_run(&self, dir: Option<&Path>, user: Option<&str>, script: &str) -> Result<()> {
        let mut cmd = Command::new(&self.program);
        cmd.arg("exec")
            .arg("-w")
            .arg(in_sandbox_path(dir.unwrap_or(Path::new("/")))?);
        if let Some(user) = user {
            cmd.args(["-u", user]);
        }
        cmd.arg(&self.name)
            .args(["--", "sh", "-c", WRAP, "sh", "sh", "-c", script]);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| self.unavailable())?;
        if !out.status.success() {
            bail!(
                "`{} exec` failed: {}",
                self.program,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// What to tell the person when Claude Code inside the sandbox isn't signed in.
    pub fn sign_in_hint(&self) -> String {
        format!(
            "Claude Code inside the sandbox is not signed in. Sign in once: run `{} exec -it {} \
             claude`, type `/login` and follow the prompts, then answer this with \
             `paja work answer <number> \"signed in\"`",
            self.program, self.name
        )
    }
}

/// The variables `cmd` sets, as the pairs to pass into the sandbox. Removals are dropped: the
/// sandbox has none of the host's variables to remove.
pub fn env_of(cmd: &Command) -> Vec<(String, String)> {
    cmd.get_envs()
        .filter_map(|(k, v)| {
            Some((
                k.to_string_lossy().into_owned(),
                v?.to_string_lossy().into_owned(),
            ))
        })
        .collect()
}

/// Whether what Claude Code printed says it isn't signed in.
pub fn signed_out(stdout: &str, stderr: &str) -> bool {
    let said = |text: &str| {
        let text = text.to_lowercase();
        text.contains("not logged in") || text.contains("please run /login")
    };
    match serde_json::from_str::<Value>(stdout) {
        Ok(value) => value["is_error"] == true && said(value["result"].as_str().unwrap_or("")),
        Err(_) => said(stdout) || said(stderr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sbx() -> Sbx {
        Sbx {
            program: "sbx".into(),
            name: "paja".into(),
        }
    }

    #[test]
    fn commands_carry_only_the_variables_they_are_given() {
        let cmd = sbx()
            .command(
                Path::new("/work"),
                &[("A".into(), "1".into())],
                &["cargo".into(), "test".into()],
            )
            .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..5], ["exec", "-w", "/work", "-e", "A=1"]);
        assert_eq!(&args[5..7], ["paja", "--"]);
        assert_eq!(&args[args.len() - 2..], ["cargo", "test"]);
    }

    #[test]
    fn unix_paths_are_unchanged() {
        if cfg!(windows) {
            return;
        }
        assert_eq!(translate("/home/a b/c").unwrap(), "/home/a b/c");
        assert_eq!(translate(r"//server/share").unwrap(), "//server/share");
    }

    #[test]
    fn windows_paths_get_the_sandbox_spelling() {
        if !cfg!(windows) {
            return;
        }
        assert_eq!(translate(r"C:\a\b").unwrap(), "/c/a/b");
        assert_eq!(translate(r"d:/a/b").unwrap(), "/d/a/b");
        assert_eq!(translate(r"C:\").unwrap(), "/c/");
        assert_eq!(translate("C:").unwrap(), "/c");
        assert_eq!(
            translate(r"C:\Users\A B\my repo").unwrap(),
            "/c/Users/A B/my repo"
        );
        assert_eq!(translate(r"\\?\C:\a").unwrap(), "/c/a");
        assert_eq!(translate("/").unwrap(), "/");
        assert_eq!(translate("/work").unwrap(), "/work");
    }

    #[test]
    fn unc_paths_are_refused_on_windows() {
        if !cfg!(windows) {
            return;
        }
        for unc in [
            r"\\server\share\a",
            r"\\?\UNC\server\share",
            "//server/share",
        ] {
            let err = translate(unc).unwrap_err().to_string();
            assert!(err.contains("UNC"), "{err}");
        }
        assert!(translate(r"C:dir").is_err());
    }

    #[test]
    fn a_signed_out_claude_is_told_apart_from_a_failed_one() {
        let out = r#"{"is_error": true, "result": "Not logged in · Please run /login"}"#;
        assert!(signed_out(out, ""));
        assert!(signed_out("", "Not logged in"));
        assert!(!signed_out(r#"{"is_error": true, "result": "boom"}"#, ""));
        assert!(!signed_out(r#"{"is_error": false, "result": "done"}"#, ""));
    }

    /// Run `apt_retry` over a fake `apt-get` (a shell function) that holds the lock for
    /// `held` calls, then succeeds. Returns whether it succeeded, its stdout and stderr.
    fn run_apt_retry(held: u32, tries: u32) -> (bool, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir
            .path()
            .join("calls")
            .display()
            .to_string()
            .replace('\\', "/");
        let script = format!(
            "{APT_RETRY}\n\
             fake_apt() {{\n  n=$(cat '{counter}' 2>/dev/null || echo 0); n=$((n + 1)); \
             echo $n > '{counter}'\n  \
             if [ \"$n\" -le {held} ]; then\n    \
             echo 'E: Could not get lock /var/lib/apt/lists/lock. It is held by process 437 (apt-get)'\n    \
             return 100\n  fi\n  echo installed\n}}\n\
             apt_retry fake_apt update"
        );
        let out = Command::new("sh")
            .args(["-c", &script])
            .env("PAJA_APT_TRIES", tries.to_string())
            .env("PAJA_APT_INTERVAL", "0")
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn apt_setup_waits_for_a_held_lock() {
        let (ok, stdout, stderr) = run_apt_retry(1, 5);
        assert!(ok, "{stderr}");
        assert!(stdout.contains("installed"), "{stdout}");
    }

    #[test]
    fn apt_setup_gives_up_naming_the_lock() {
        let (ok, _, stderr) = run_apt_retry(u32::MAX, 3);
        assert!(!ok);
        assert!(
            stderr.contains("/var/lib/apt/lists/lock") && stderr.contains("still held"),
            "{stderr}"
        );
    }

    #[test]
    fn setup_runs_apt_through_the_retry() {
        let script = setup_root();
        assert!(script.contains("apt_retry apt-get update"), "{script}");
        assert!(script.contains("apt_retry apt-get install"), "{script}");
    }

    #[test]
    fn an_unconfigured_sandbox_is_none() {
        // The variable is unset in the test environment; `start` must not try to run anything.
        if configured().is_none() {
            assert!(start(&[]).unwrap().is_none());
        }
    }
}
