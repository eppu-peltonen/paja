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
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The environment variable that names the `sbx` program and turns the sandbox on.
pub const PROGRAM_VAR: &str = "PAJA_SBX";

/// The sandbox's name. One per machine; it is reused by every run.
const NAME: &str = "paja";

/// Names another sandbox than `paja`. Tests that run a real `sbx` set it, so they never touch
/// the sandbox the person works in.
pub const NAME_VAR: &str = "PAJA_SBX_NAME";

/// Where code in the sandbox keeps its throwaway `PAJA_HOME`: the sandbox's own `/tmp`, never a
/// path on the host.
pub const SCRATCH: &str = "/tmp/paja-home";

/// The variables that tell the git shim where the worktree is, as the sandbox spells its paths.
const WORKTREE_ROOT_VAR: &str = "PAJA_WT_ROOT";
const WORKTREE_GITDIR_VAR: &str = "PAJA_WT_GITDIR";

/// A `git` that runs in the sandbox ahead of the real one when the worktree's `.git` file holds a
/// host path (`C:/.../.git/worktrees/x`) the sandbox can't resolve. A git command aimed at the
/// worktree (the current directory or the `-C` directories are inside it, after any other global
/// options) gets `--git-dir` and `--work-tree` in the sandbox's spelling; when the caller chose
/// its own work tree (`--work-tree` or a `GIT_WORK_TREE` naming another directory) it gets only
/// `--git-dir`, the repository git would find there itself. A caller that chose its own
/// repository (`--git-dir` or a `GIT_DIR` naming another directory) gets nothing added.
///
/// A git directory passed this way reaches the processes git starts as `GIT_DIR` and
/// `GIT_WORK_TREE`, so a nested git that does reach the shim could inherit the worktree's. Each is
/// decided on its own: an inherited `GIT_DIR` that is the worktree's git directory is cleared for
/// a command aimed outside the worktree, and an inherited `GIT_WORK_TREE` that is the worktree is
/// cleared for a command aimed outside it or at another repository of the caller's choosing,
/// and so is an inherited `GIT_INDEX_FILE` in the worktree's git directory. A value naming any
/// other directory is the caller's own and left alone. A caller that sets any of them to
/// the worktree's own can't be told from git handing it down, so it is treated the same way.
/// Git puts its own exec directory first on `PATH` for hooks and `!` aliases, so a git called
/// there usually runs the real git, not this shim. A relative value is read, as git reads it,
/// against the directory after any `-C`; git hands down absolute ones, except the `.` it gives
/// a hook as `GIT_WORK_TREE`, which a hook that runs git in another directory makes the
/// caller's own. Paths are compared by identity (`-ef`: each directory from the target up to
/// `/` against the root, and the git directories), not by spelling, so separators, case, short
/// names (`RUNNER~1`), symlinks and mount spellings don't matter.
const GIT_SHIM: &str = r#"#!/bin/sh
dir=.
want=0
skip=0
gd=none
wt=none
fgd=0
fwt=0
gdv=
ix=none
for a in "$@"; do
  if [ "$want" = 2 ]; then
    gdv=$a
    want=0
    continue
  fi
  if [ "$want" = 1 ]; then
    dir=$(cd "$dir" 2>/dev/null && cd "$a" 2>/dev/null && pwd -P) || dir=/nonexistent-paja
    want=0
    continue
  fi
  if [ "$skip" = 1 ]; then
    skip=0
    continue
  fi
  case "$a" in
    -C) want=1 ;;
    -c|--namespace|--super-prefix|--config-env) skip=1 ;;
    --git-dir) fgd=1; want=2 ;;
    --work-tree) fwt=1; skip=1 ;;
    --git-dir=*) fgd=1; gdv=${a#--git-dir=} ;;
    --work-tree=*) fwt=1 ;;
    -*) ;;
    *) break ;;
  esac
done
# Git reads a relative value after any `-C`.
at() { case "$1" in /*) printf '%s' "$1" ;; *) printf '%s' "$dir/$1" ;; esac; }
if [ -n "${GIT_DIR:-}" ]; then
  if [ "$GIT_DIR" = "$PAJA_WT_GITDIR" ] || [ "$(at "$GIT_DIR")" -ef "$PAJA_WT_GITDIR" ]; then gd=inherited; else gd=own; fi
fi
if [ -n "${GIT_WORK_TREE:-}" ]; then
  if [ "$GIT_WORK_TREE" = "$PAJA_WT_ROOT" ] || [ "$(at "$GIT_WORK_TREE")" -ef "$PAJA_WT_ROOT" ]; then wt=inherited; else wt=own; fi
fi
if [ -n "${GIT_INDEX_FILE:-}" ] && [ "$(dirname "$(at "$GIT_INDEX_FILE")")" -ef "$PAJA_WT_GITDIR" ]; then ix=inherited; fi
if [ "$fgd" = 1 ] || [ "$gd" = own ]; then
  # The caller chose a repository: the worktree's inherited work tree and index belong to it
  # only when that is the worktree's own git directory.
  chosen=${gdv:-$GIT_DIR}
  if [ "$fgd" = 0 ] || { [ "$chosen" != "$PAJA_WT_GITDIR" ] && ! [ "$(at "$chosen")" -ef "$PAJA_WT_GITDIR" ]; }; then
    if [ "$wt" = inherited ]; then unset GIT_WORK_TREE; fi
    if [ "$ix" = inherited ]; then unset GIT_INDEX_FILE; fi
  fi
  exec "$PAJA_REAL_GIT" "$@"
fi
p=$(cd "$dir" 2>/dev/null && pwd -P)
while [ -n "$p" ]; do
  if [ "$p" -ef "$PAJA_WT_ROOT" ]; then
    if [ "$fwt" = 1 ] || [ "$wt" = own ]; then
      exec "$PAJA_REAL_GIT" --git-dir="$PAJA_WT_GITDIR" "$@"
    fi
    exec "$PAJA_REAL_GIT" --git-dir="$PAJA_WT_GITDIR" --work-tree="$PAJA_WT_ROOT" "$@"
  fi
  [ "$p" = / ] && break
  p=$(dirname "$p")
done
if [ "$gd" = inherited ]; then unset GIT_DIR; fi
if [ "$wt" = inherited ]; then unset GIT_WORK_TREE; fi
if [ "$ix" = inherited ]; then unset GIT_INDEX_FILE; fi
exec "$PAJA_REAL_GIT" "$@""#;

/// Puts rustup's directory on `PATH`, installs the git shim when the worktree needs one, then
/// runs the command it is given.
fn wrap() -> String {
    format!(
        r#"PATH="$HOME/.cargo/bin:$PATH"; export PATH
if [ -n "${{{WORKTREE_ROOT_VAR}:-}}" ] && PAJA_REAL_GIT=$(command -v git) && shim="$HOME/.paja-shim" && mkdir -p "$shim"; then
  cat > "$shim/git.$$" <<'PAJA_SHIM_END'
{GIT_SHIM}
PAJA_SHIM_END
  chmod +x "$shim/git.$$" && mv -f "$shim/git.$$" "$shim/git" && export PAJA_REAL_GIT && PATH="$shim:$PATH" && export PATH
fi
exec "$@""#
    )
}

/// The worktree root and its git directory, both as the sandbox spells them, when `dot_git` (the
/// content of the worktree's `.git` file) points at a host path. `None` for a plain repository,
/// a relative gitdir (it resolves the same in the sandbox) or a host whose paths need no
/// translation.
fn redirect_from(root: &str, dot_git: &str) -> Result<Option<(String, String)>> {
    let Some(gitdir) = dot_git
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
    else {
        return Ok(None);
    };
    let translated = translate(gitdir)?;
    if translated == gitdir {
        return Ok(None);
    }
    Ok(Some((root.to_string(), translated)))
}

/// A shell function that runs a command (an `apt-get`), retrying while apt's lists or dpkg lock
/// is held. A fresh sandbox runs its own `apt-get` just after it starts. It tries
/// `$PAJA_APT_TRIES` times (default 60) `$PAJA_APT_INTERVAL` seconds apart (default 5), about
/// five minutes, then fails with apt's message, which names the lock. The output streams to
/// stdout as the command writes it. Tests override both variables.
const APT_RETRY: &str = r#"apt_retry() {
  tries=0
  max="${PAJA_APT_TRIES:-60}"
  log=$(mktemp) || return 1
  while :; do
    rm -f "$log.rc"
    # The output streams as it comes and is kept in the log; the status goes in a file, since
    # a pipe hides it.
    { "$@" 2>&1 || echo "$?" > "$log.rc"; } | tee "$log"
    if [ ! -e "$log.rc" ]; then
      rm -f "$log"
      return 0
    fi
    out=$(cat "$log")
    case "$out" in
      *"Could not get lock"*|*"Unable to lock"*)
        tries=$((tries + 1))
        if [ "$tries" -ge "$max" ]; then
          rm -f "$log" "$log.rc"
          printf '%s\n' "$out" >&2
          echo "apt's lock is still held after $tries tries (the lock is named above); wait for the other apt-get or dpkg to finish and run again" >&2
          return 1
        fi
        sleep "${PAJA_APT_INTERVAL:-5}"
        ;;
      *)
        rm -f "$log" "$log.rc"
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

thread_local! {
    // One slot per thread, not per process: a long-lived caller such as `paja mcp` runs each
    // call on a thread of its own, so two calls at once never start an agent in each other's
    // sandbox, or in none once the other has finished.
    static ACTIVE: RefCell<Option<Sbx>> = const { RefCell::new(None) };
}

/// Use `sandbox` (or none) for the agents and checks this thread starts until the returned
/// guard is dropped, which puts back the sandbox in use before, on every way out. Guards are
/// dropped in the reverse order they were entered, as they are when each is bound to a local;
/// the guard can't be sent to another thread.
#[must_use = "the sandbox is in use only while the guard lives"]
pub fn enter(sandbox: Option<Sbx>) -> Entered {
    Entered {
        before: ACTIVE.with(|slot| slot.replace(sandbox)),
        _thread: std::marker::PhantomData,
    }
}

/// The sandbox in use on this thread, if the step started one.
pub fn active() -> Option<Sbx> {
    ACTIVE.with(|slot| slot.borrow().clone())
}

/// What [`enter`] returns: while it lives, its sandbox is the thread's.
pub struct Entered {
    before: Option<Sbx>,
    // Not `Send`: dropped on another thread, it would set that thread's slot instead.
    _thread: std::marker::PhantomData<*const ()>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        let before = self.before.take();
        ACTIVE.with(|slot| *slot.borrow_mut() = before);
    }
}

/// The `sbx` program, when the person turned the sandbox on.
pub fn configured() -> Option<String> {
    std::env::var(PROGRAM_VAR)
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
}

/// The project's setup script, read from the default branch.
pub const SETUP_FILE: &str = ".paja/setup";

/// Where the sandbox keeps the hash of the `.paja/setup` script that last succeeded in it, so
/// the script runs again only when it changes. It lives in the sandbox's home: it describes the
/// sandbox, so it goes away with it.
const SETUP_MARKER: &str = "$HOME/.paja-setup-hash";

/// How many of a failed setup script's last output lines go into the error.
const SETUP_TAIL_LINES: usize = 20;

/// A stable hash (FNV-1a, 64 bits) of `text`, as hex. It only has to notice a changed script.
fn script_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// Start (or reuse) the sandbox with `mounts` reachable inside it. `Ok(None)` when it isn't
/// configured; an error, never a silent fallback, when it is configured but can't start.
/// `setup` is the project's `.paja/setup` from the default branch; without it the sandbox gets
/// the Rust toolchain.
pub fn start(mounts: &[PathBuf], setup: Option<&str>) -> Result<Option<Sbx>> {
    let Some(program) = configured() else {
        return Ok(None);
    };
    let sbx = Sbx {
        program,
        name: std::env::var(NAME_VAR)
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| NAME.to_string()),
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
    if let Some(script) = setup {
        sbx.run_setup(script)?;
    } else if sbx.exec_ok(TOOLCHAIN_CHECK).is_err() {
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

fn translate(path: &str) -> Result<String> {
    if cfg!(windows) {
        windows_spelling(path)
    } else {
        Ok(path.to_string())
    }
}

/// The sandbox spelling of a Windows host path. Separate from `translate` so its rules are
/// tested on every platform, not only on Windows.
fn windows_spelling(path: &str) -> Result<String> {
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

/// The script that succeeds when Claude Code's transcript of session `id` is in the home.
fn transcript_script(id: &str) -> String {
    format!("ls \"$HOME\"/.claude/projects/*/{id}.jsonl >/dev/null 2>&1")
}

/// A prompt written to a file the sandbox can read, for an agent's stdin. `sbx exec -i` can
/// hang for good when the program in the sandbox exits before it has read all of its stdin (the
/// sandbox's stdin copy fails with a broken pipe and the exec never returns), so Paja never
/// pipes a prompt through it. The file is removed when this is dropped.
pub struct PromptFile {
    host: PathBuf,
    in_sandbox: String,
}

impl PromptFile {
    /// Write `prompt` to a new file under `dir`, a folder the sandbox mounts (the repository's
    /// git data), in `paja-prompts/`.
    pub fn write(dir: &Path, prompt: &str) -> Result<PromptFile> {
        static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let folder = dir.join("paja-prompts");
        std::fs::create_dir_all(&folder)
            .with_context(|| format!("creating {} for an agent's prompt", folder.display()))?;
        let n = COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let host = folder.join(format!("{}-{n}.txt", std::process::id()));
        std::fs::write(&host, prompt)
            .with_context(|| format!("writing an agent's prompt to {}", host.display()))?;
        let in_sandbox = match in_sandbox_path(&host) {
            Ok(path) => path,
            Err(err) => {
                let _ = std::fs::remove_file(&host);
                return Err(err);
            }
        };
        Ok(PromptFile { host, in_sandbox })
    }

    /// `argv` run with this file on its stdin, by a shell in the sandbox.
    pub fn feeding(&self, argv: &[String]) -> Vec<String> {
        let mut fed = vec![
            "sh".to_string(),
            "-c".to_string(),
            "exec \"$@\" < \"$0\"".to_string(),
            self.in_sandbox.clone(),
        ];
        fed.extend(argv.iter().cloned());
        fed
    }
}

impl Drop for PromptFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.host);
    }
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
        let root = in_sandbox_path(dir)?;
        cmd.arg("exec").arg("-w").arg(&root);
        for (key, value) in env {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        // A worktree's `.git` file holds a host path the sandbox can't resolve; the shim
        // gives git the sandbox's spelling of it.
        if let Ok(dot_git) = std::fs::read_to_string(dir.join(".git")) {
            if let Some((root, gitdir)) = redirect_from(&root, &dot_git)? {
                cmd.arg("-e").arg(format!("{WORKTREE_ROOT_VAR}={root}"));
                cmd.arg("-e").arg(format!("{WORKTREE_GITDIR_VAR}={gitdir}"));
            }
        }
        cmd.arg(&self.name)
            .args(["--", "sh", "-c", &wrap(), "sh"])
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
            && self.exec_ok(&transcript_script(id)).is_ok()
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

    /// Run the project's `.paja/setup` once as the sandbox user, again only when it changed.
    /// A C toolchain, `curl` and `git` come first when the sandbox lacks them, since the user
    /// can't install packages. A failing script is an error carrying the end of its output.
    fn run_setup(&self, script: &str) -> Result<()> {
        let hash = script_hash(script);
        let marker = format!("[ \"$(cat \"{SETUP_MARKER}\" 2>/dev/null)\" = \"{hash}\" ]");
        if self.exec_ok(&marker).is_ok() {
            return Ok(());
        }
        let fix = format!(
            "fix `.paja/setup` on the default branch and run again; to look around, run `{} exec \
             -it {} sh`",
            self.program, self.name
        );
        let base = "command -v cc >/dev/null 2>&1 && command -v curl >/dev/null 2>&1";
        if self.exec_ok(base).is_err() {
            self.exec_as(Some("root"), &setup_root()).with_context(|| {
                format!("installing a C toolchain in the sandbox `{}`", self.name)
            })?;
        }
        eprintln!(
            "running {SETUP_FILE} in the sandbox `{}` (a first run can take minutes)",
            self.name
        );
        // Stderr goes into stdout inside the sandbox, so the tail keeps the order they came in.
        // Same line, so the script's line numbers are unchanged.
        let out = self.exec_output(None, None, &format!("exec 2>&1; {script}"))?;
        if !out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(SETUP_TAIL_LINES)..].join("\n");
            bail!(
                "`.paja/setup` failed in the sandbox `{}` ({}); its last output:\n{}\n{fix}",
                self.name,
                out.status,
                tail
            );
        }
        self.exec_ok(&format!("printf '%s\\n' \"{hash}\" > \"{SETUP_MARKER}\""))
            .with_context(|| {
                format!(
                    "`.paja/setup` ran but the sandbox `{}` won't record it; {}",
                    self.name,
                    self.fix()
                )
            })
    }

    fn exec_run(&self, dir: Option<&Path>, user: Option<&str>, script: &str) -> Result<()> {
        let out = self.exec_output(dir, user, script)?;
        if !out.status.success() {
            bail!(
                "`{} exec` failed: {}",
                self.program,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    fn exec_output(
        &self,
        dir: Option<&Path>,
        user: Option<&str>,
        script: &str,
    ) -> Result<std::process::Output> {
        let mut cmd = Command::new(&self.program);
        cmd.arg("exec")
            .arg("-w")
            .arg(in_sandbox_path(dir.unwrap_or(Path::new("/")))?);
        if let Some(user) = user {
            cmd.args(["-u", user]);
        }
        cmd.arg(&self.name)
            .args(["--", "sh", "-c", &wrap(), "sh", "sh", "-c", script]);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .with_context(|| self.unavailable())?;
        Ok(out)
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

    fn named(name: &str) -> Option<Sbx> {
        Some(Sbx {
            program: "sbx".into(),
            name: name.into(),
        })
    }

    fn active_name() -> Option<String> {
        active().map(|s| s.name)
    }

    #[test]
    fn a_sandbox_is_in_use_while_its_guard_lives_and_the_one_before_comes_back() {
        assert!(active().is_none());
        let outer = enter(named("outer"));
        assert_eq!(active_name().as_deref(), Some("outer"));
        {
            let _none = enter(None);
            assert!(active().is_none());
        }
        assert_eq!(active_name().as_deref(), Some("outer"));
        // An early return (here a `?`) drops the guard too, so the slot is never left set.
        let step = || -> Result<()> {
            let _inner = enter(named("inner"));
            assert_eq!(active_name().as_deref(), Some("inner"));
            anyhow::bail!("the agent failed")
        };
        assert!(step().is_err());
        assert_eq!(active_name().as_deref(), Some("outer"));
        drop(outer);
        assert!(active().is_none());
    }

    #[test]
    fn two_threads_at_once_each_keep_their_own_sandbox() {
        // As two `paja mcp` tool calls do: each enters its sandbox, and the other's entering
        // and leaving in between changes nothing for it.
        let (a_in, b_in) = (
            std::sync::Arc::new(std::sync::Barrier::new(2)),
            std::sync::Arc::new(std::sync::Barrier::new(2)),
        );
        let (a_out, b_out) = (a_in.clone(), b_in.clone());
        let a = std::thread::spawn(move || {
            let _a = enter(named("a"));
            a_out.wait(); // b enters its own
            b_out.wait(); // b has left its own
            active_name()
        });
        let b = std::thread::spawn(move || {
            a_in.wait();
            let first = {
                let _b = enter(named("b"));
                active_name()
            };
            b_in.wait();
            (first, active_name())
        });
        assert_eq!(a.join().unwrap().as_deref(), Some("a"));
        let (first, after) = b.join().unwrap();
        assert_eq!(first.as_deref(), Some("b"));
        assert_eq!(after, None);
    }

    #[test]
    fn a_prompt_file_feeds_the_agent_and_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = PromptFile::write(dir.path(), "the prompt").unwrap();
        let host = file.host.clone();
        assert_eq!(std::fs::read_to_string(&host).unwrap(), "the prompt");
        let argv = file.feeding(&["claude".to_string(), "-p".to_string()]);
        assert_eq!(argv[..3], ["sh", "-c", "exec \"$@\" < \"$0\""]);
        assert_eq!(argv[3], in_sandbox_path(&host).unwrap());
        assert_eq!(argv[4..], ["claude", "-p"]);
        let second = PromptFile::write(dir.path(), "another").unwrap();
        assert_ne!(second.host, host);
        drop(file);
        assert!(!host.exists());
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
    fn translate_uses_the_host_platform_rules() {
        if cfg!(windows) {
            assert_eq!(translate(r"C:\a").unwrap(), "/c/a");
            assert!(translate(r"\\server\share").is_err());
            return;
        }
        assert_eq!(translate("/home/a b/c").unwrap(), "/home/a b/c");
        assert_eq!(translate(r"//server/share").unwrap(), "//server/share");
    }

    #[test]
    fn windows_paths_get_the_sandbox_spelling() {
        assert_eq!(windows_spelling(r"C:\a\b").unwrap(), "/c/a/b");
        assert_eq!(windows_spelling(r"d:/a/b").unwrap(), "/d/a/b");
        assert_eq!(windows_spelling(r"C:\").unwrap(), "/c/");
        assert_eq!(windows_spelling("C:").unwrap(), "/c");
        assert_eq!(
            windows_spelling(r"C:\Users\A B\my repo").unwrap(),
            "/c/Users/A B/my repo"
        );
        assert_eq!(windows_spelling(r"\\?\C:\a").unwrap(), "/c/a");
        assert_eq!(windows_spelling("/").unwrap(), "/");
        assert_eq!(windows_spelling("/work").unwrap(), "/work");
    }

    #[test]
    fn windows_unc_paths_are_refused() {
        for unc in [
            r"\\server\share\a",
            r"\\?\UNC\server\share",
            "//server/share",
        ] {
            let err = windows_spelling(unc).unwrap_err().to_string();
            assert!(err.contains("UNC"), "{err}");
        }
        assert!(windows_spelling(r"C:dir").is_err());
    }

    #[test]
    fn a_host_gitdir_gets_the_sandbox_spelling() {
        let dot_git = "gitdir: C:/repo/.git/worktrees/w\n";
        if cfg!(windows) {
            assert_eq!(
                redirect_from("/c/envs/w", dot_git).unwrap(),
                Some((
                    "/c/envs/w".to_string(),
                    "/c/repo/.git/worktrees/w".to_string()
                ))
            );
        }
        // A relative gitdir resolves the same in the sandbox; a plain checkout has no file.
        assert_eq!(
            redirect_from("/w", "gitdir: ../repo/.git/worktrees/w\n").unwrap(),
            None
        );
        assert_eq!(redirect_from("/w", "not a gitdir file").unwrap(), None);
    }

    /// Run the git shim with a fake real git that prints the `GIT_DIR` and `GIT_WORK_TREE` it
    /// gets, from `cwd`, for worktree root `root_s` (as the sandbox would spell it).
    fn shim_sees(root_s: &str, cwd: &Path, args: &[&str]) -> String {
        shim_sees_with(root_s, cwd, args, None)
    }

    /// Like `shim_sees`, with `GIT_DIR` set in the shim's environment when given.
    fn shim_sees_with(root_s: &str, cwd: &Path, args: &[&str], git_dir: Option<&str>) -> String {
        let env: Vec<(&str, &str)> = git_dir.map(|g| ("GIT_DIR", g)).into_iter().collect();
        shim_sees_in(root_s, "/gitdirs/w", cwd, args, &env)
    }

    /// Like `shim_sees`, for the worktree git directory `gitdir_s` and with `env` set.
    fn shim_sees_in(
        root_s: &str,
        gitdir_s: &str,
        cwd: &Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> String {
        let tools = tempfile::tempdir().unwrap();
        let slash = |p: &Path| p.display().to_string().replace('\\', "/");
        let shim = tools.path().join("shim");
        let real = tools.path().join("real");
        std::fs::write(&shim, GIT_SHIM).unwrap();
        std::fs::write(
            &real,
            "#!/bin/sh\ndir=; tree=; wt=\nfor a in \"$@\"; do case \"$a\" in --git-dir=*) dir=${a#--git-dir=} ;; --work-tree=*) tree=${a#--work-tree=} ;; esac; done\nif [ -n \"${GIT_WORK_TREE:-}\" ]; then wt=\" wt=$GIT_WORK_TREE\"; fi\nif [ -n \"${GIT_INDEX_FILE:-}\" ]; then wt=\"$wt ix=$GIT_INDEX_FILE\"; fi\necho \"dir=$dir tree=$tree env=${GIT_DIR:-}$wt\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cmd = Command::new("sh");
        cmd.arg(slash(&shim))
            .args(args)
            .current_dir(cwd)
            .env("PAJA_REAL_GIT", slash(&real))
            .env(WORKTREE_ROOT_VAR, root_s)
            .env(WORKTREE_GITDIR_VAR, gitdir_s)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .envs(env.iter().copied());
        let out = cmd.output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `root` as the shell spells it, so the test doesn't depend on the host's path style.
    fn shell_path(root: &Path) -> String {
        if cfg!(windows) {
            windows_spelling(&root.to_string_lossy()).unwrap()
        } else {
            root.to_string_lossy().into_owned()
        }
    }

    #[test]
    fn the_git_shim_redirects_only_git_commands_aimed_at_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        let other = tempfile::tempdir().unwrap();
        let root_s = shell_path(root.path());
        let hit = |out: String| out == format!("dir=/gitdirs/w tree={root_s} env=");
        let miss = |out: String| out == "dir= tree= env=";

        assert!(hit(shim_sees(&root_s, root.path(), &["status"])));
        assert!(hit(shim_sees(&root_s, &sub, &["status"])));
        assert!(miss(shim_sees(&root_s, other.path(), &["status"])));
        // `-C` moves the command into or out of the worktree.
        let sub_s = shell_path(&sub);
        let other_s = shell_path(other.path());
        assert!(hit(shim_sees(
            &root_s,
            other.path(),
            &["-C", &sub_s, "status"]
        )));
        assert!(miss(shim_sees(
            &root_s,
            root.path(),
            &["-C", &other_s, "status"]
        )));
        // Other global options before `-C` don't stop the scan.
        assert!(hit(shim_sees(
            &root_s,
            other.path(),
            &["-c", "k=v", "--no-pager", "-C", &sub_s, "status"]
        )));
        assert!(miss(shim_sees(
            &root_s,
            root.path(),
            &["--no-pager", "-c", "k=v", "-C", &other_s, "status"]
        )));
        // A `-C` after the subcommand belongs to the subcommand.
        assert!(miss(shim_sees(
            &root_s,
            other.path(),
            &["log", "-C", &sub_s]
        )));
        // A git directory the caller chose is left alone.
        assert!(miss(shim_sees(
            &root_s,
            root.path(),
            &["--git-dir", "/x", "status"]
        )));
    }

    #[test]
    fn the_git_shim_clears_an_inherited_git_dir_outside_the_worktree_only() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root_s = shell_path(root.path());
        let other_s = shell_path(other.path());
        let inherited = Some("/gitdirs/w");
        let outside = "dir= tree= env=";
        // Git handed the worktree's git directory to a nested git aimed elsewhere: cleared.
        assert_eq!(
            shim_sees_with(&root_s, other.path(), &["status"], inherited),
            outside
        );
        assert_eq!(
            shim_sees_with(&root_s, root.path(), &["-C", &other_s, "status"], inherited),
            outside
        );
        // Inside the worktree it is still redirected.
        assert_eq!(
            shim_sees_with(&root_s, root.path(), &["status"], inherited),
            format!("dir=/gitdirs/w tree={root_s} env=/gitdirs/w")
        );
        // A caller's own GIT_DIR is left alone, inside the worktree or out.
        assert_eq!(
            shim_sees_with(&root_s, root.path(), &["status"], Some("/x")),
            "dir= tree= env=/x"
        );
        assert_eq!(
            shim_sees_with(&root_s, other.path(), &["status"], Some("/x")),
            "dir= tree= env=/x"
        );
    }

    #[test]
    fn the_git_shim_knows_the_inherited_git_directory_and_work_tree_by_identity() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let gitdirs = tempfile::tempdir().unwrap();
        let gitdir = gitdirs.path().join("w");
        std::fs::create_dir(&gitdir).unwrap();
        let root_s = shell_path(root.path());
        let gitdir_s = shell_path(&gitdir);
        // The same directories, spelled another way.
        let gitdir_other = format!("{}/./w", shell_path(gitdirs.path()));
        let root_other = format!("{root_s}/.");
        let sees = |cwd: &Path, env: &[(&str, &str)]| {
            shim_sees_in(&root_s, &gitdir_s, cwd, &["status"], env)
        };
        let outside = "dir= tree= env=";
        // Git handed the worktree's git directory down in another spelling: still cleared.
        assert_eq!(sees(other.path(), &[("GIT_DIR", &gitdir_other)]), outside);
        // An inherited GIT_WORK_TREE, with GIT_DIR or on its own, is cleared too.
        assert_eq!(
            sees(
                other.path(),
                &[("GIT_DIR", &gitdir_s), ("GIT_WORK_TREE", &root_s)]
            ),
            outside
        );
        assert_eq!(
            sees(other.path(), &[("GIT_WORK_TREE", &root_other)]),
            outside
        );
        // Inside the worktree it is redirected, the variable left as it was.
        assert_eq!(
            sees(root.path(), &[("GIT_WORK_TREE", &root_s)]),
            format!("dir={gitdir_s} tree={root_s} env= wt={root_s}")
        );
        // A work tree the caller chose is left alone; inside the worktree the command still gets
        // the worktree's git directory, which git would find there itself.
        let other_s = shell_path(other.path());
        assert_eq!(
            sees(root.path(), &[("GIT_WORK_TREE", &other_s)]),
            format!("dir={gitdir_s} tree= env= wt={other_s}")
        );
        assert_eq!(
            sees(other.path(), &[("GIT_WORK_TREE", &other_s)]),
            format!("dir= tree= env= wt={other_s}")
        );
        // Each variable is decided on its own: an inherited GIT_DIR beside the caller's own
        // GIT_WORK_TREE, or beside `--work-tree`, is still cleared outside the worktree.
        assert_eq!(
            sees(
                other.path(),
                &[("GIT_DIR", &gitdir_s), ("GIT_WORK_TREE", &other_s)]
            ),
            format!("dir= tree= env= wt={other_s}")
        );
        let flag = format!("--work-tree={other_s}");
        assert_eq!(
            shim_sees_in(
                &root_s,
                &gitdir_s,
                other.path(),
                &[&flag, "status"],
                &[("GIT_DIR", &gitdir_s)]
            ),
            format!("dir= tree={other_s} env=")
        );
        // Inside the worktree an inherited GIT_DIR in another spelling is still redirected.
        assert_eq!(
            sees(root.path(), &[("GIT_DIR", &gitdir_other)]),
            format!("dir={gitdir_s} tree={root_s} env={gitdir_other}")
        );
        // A repository the caller chose keeps its GIT_DIR, and loses the worktree's inherited
        // work tree, which isn't its.
        assert_eq!(
            sees(
                root.path(),
                &[("GIT_DIR", "/x"), ("GIT_WORK_TREE", &root_s)]
            ),
            "dir= tree= env=/x"
        );
        // So does `--git-dir` naming another repository, in either form; one naming the
        // worktree's own git directory keeps the work tree that goes with it.
        let flagged = |args: &[&str], env: &[(&str, &str)]| {
            shim_sees_in(&root_s, &gitdir_s, root.path(), args, env)
        };
        let inherited_tree = [("GIT_WORK_TREE", root_s.as_str())];
        assert_eq!(
            flagged(&["--git-dir=/x", "status"], &inherited_tree),
            "dir=/x tree= env="
        );
        assert_eq!(
            flagged(&["--git-dir", "/x", "status"], &inherited_tree),
            "dir= tree= env="
        );
        let own_dir = format!("--git-dir={gitdir_s}");
        assert_eq!(
            flagged(&[&own_dir, "status"], &inherited_tree),
            format!("dir={gitdir_s} tree= env= wt={root_s}")
        );
        let index = format!("{gitdir_s}/index");
        assert_eq!(
            flagged(
                &[&own_dir, "status"],
                &[("GIT_WORK_TREE", &root_s), ("GIT_INDEX_FILE", &index)]
            ),
            format!("dir={gitdir_s} tree= env= wt={root_s} ix={index}")
        );
        // A relative `GIT_WORK_TREE` is read after `-C` too: a hook's `.` is the worktree only
        // for a git run there.
        let other_s = other.path().to_str().unwrap();
        assert_eq!(
            flagged(&["-C", other_s, "status"], &[("GIT_WORK_TREE", ".")]),
            "dir= tree= env= wt=."
        );
        assert_eq!(
            flagged(&["status"], &[("GIT_WORK_TREE", ".")]),
            format!("dir={gitdir_s} tree={root_s} env= wt=.")
        );
        // A relative `--git-dir` is read after `-C`, as git reads it.
        let gitdirs_s = gitdirs.path().to_str().unwrap();
        assert_eq!(
            flagged(&["-C", gitdirs_s, "--git-dir=w", "status"], &inherited_tree),
            format!("dir=w tree= env= wt={root_s}")
        );
        assert_eq!(
            flagged(&["--git-dir", &gitdir_other, "status"], &inherited_tree),
            format!("dir= tree= env= wt={root_s}")
        );
        // An index file git handed down from the worktree's git directory goes with it.
        assert_eq!(
            sees(
                other.path(),
                &[("GIT_DIR", &gitdir_s), ("GIT_INDEX_FILE", &index)]
            ),
            outside
        );
        assert_eq!(
            sees(
                root.path(),
                &[("GIT_DIR", "/x"), ("GIT_INDEX_FILE", &index)]
            ),
            "dir= tree= env=/x"
        );
        // Inside the worktree it stays, beside the redirect.
        assert_eq!(
            sees(root.path(), &[("GIT_INDEX_FILE", &index)]),
            format!("dir={gitdir_s} tree={root_s} env= ix={index}")
        );
        // A symlink to the worktree's git directory is the same directory.
        #[cfg(unix)]
        {
            let link = gitdirs.path().join("link");
            std::os::unix::fs::symlink(&gitdir, &link).unwrap();
            let link_s = shell_path(&link);
            assert_eq!(sees(other.path(), &[("GIT_DIR", &link_s)]), outside);
        }
    }

    #[test]
    fn the_wrapper_installs_the_shim_only_for_a_worktree_that_needs_one() {
        let script = wrap();
        assert!(script.contains(WORKTREE_ROOT_VAR) && script.contains("PAJA_SHIM_END"));
        assert!(script.trim_end().ends_with(r#"exec "$@""#));
        let cmd = sbx()
            .command(Path::new("/work"), &[], &["true".into()])
            .unwrap();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|a| a.starts_with(WORKTREE_ROOT_VAR)));
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
    fn the_transcript_script_finds_a_session_under_any_project() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude").join("projects").join("-work");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("abc-123.jsonl"), "{}\n").unwrap();
        let found = |id: &str| {
            Command::new("sh")
                .args(["-c", &transcript_script(id)])
                .env("HOME", home.path())
                .status()
                .unwrap()
                .success()
        };
        assert!(found("abc-123"));
        assert!(!found("other-id"));
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
            assert!(start(&[], None).unwrap().is_none());
        }
    }
}
