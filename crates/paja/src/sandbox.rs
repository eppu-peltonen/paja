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

/// Installs a C toolchain and Rust (what Vasara's checks need) when the sandbox lacks them.
const SETUP: &str = r#"set -e
S=""; [ "$(id -u)" = 0 ] || S="sudo -n"
if ! command -v cc >/dev/null 2>&1; then
  $S apt-get update && $S apt-get install -y build-essential curl ca-certificates git
fi
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
    if sbx.exec_ok(TOOLCHAIN_CHECK).is_err() {
        sbx.exec_ok(SETUP).with_context(|| {
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
    pub fn command(&self, dir: &Path, env: &[(String, String)], argv: &[String]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.arg("exec").arg("-w").arg(dir);
        for (key, value) in env {
            cmd.arg("-e").arg(format!("{key}={value}"));
        }
        cmd.arg(&self.name)
            .args(["--", "sh", "-c", WRAP, "sh"])
            .args(argv);
        cmd
    }

    /// The command that runs the shell script `script` (a check) in the sandbox.
    pub fn script(&self, dir: &Path, env: &[(String, String)], script: &str) -> Command {
        self.command(
            dir,
            env,
            &["sh".to_string(), "-c".to_string(), script.to_string()],
        )
    }

    fn exec_ok(&self, script: &str) -> Result<()> {
        let out = self
            .script(Path::new("/"), &[], script)
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
        let cmd = sbx().command(
            Path::new("/work"),
            &[("A".into(), "1".into())],
            &["cargo".into(), "test".into()],
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..5], ["exec", "-w", "/work", "-e", "A=1"]);
        assert_eq!(&args[5..7], ["paja", "--"]);
        assert_eq!(&args[args.len() - 2..], ["cargo", "test"]);
    }

    #[test]
    fn a_signed_out_claude_is_told_apart_from_a_failed_one() {
        let out = r#"{"is_error": true, "result": "Not logged in · Please run /login"}"#;
        assert!(signed_out(out, ""));
        assert!(signed_out("", "Not logged in"));
        assert!(!signed_out(r#"{"is_error": true, "result": "boom"}"#, ""));
        assert!(!signed_out(r#"{"is_error": false, "result": "done"}"#, ""));
    }

    #[test]
    fn an_unconfigured_sandbox_is_none() {
        // The variable is unset in the test environment; `start` must not try to run anything.
        if configured().is_none() {
            assert!(start(&[]).unwrap().is_none());
        }
    }
}
