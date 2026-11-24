//! The deny-list hook: a Claude Code `PreToolUse` hook that every agent Mestari starts runs
//! under. It blocks tool calls that could reach outside the agent's environment: pushing or
//! talking to GitHub, the network, secrets, other branches, and files outside the worktree.
//!
//! It fails closed. `paja hook` exits 2 (block) when a rule matches and also when anything goes
//! wrong while deciding, such as unreadable input. Mestari runs a self-test before each agent
//! and refuses to start one if the hook doesn't block.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

/// What Claude Code sends a `PreToolUse` hook on stdin.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Value,
    /// The agent's working directory: its Ahjo environment.
    pub cwd: PathBuf,
}

/// Commands and words that never run inside an agent. Matched as whole words in each
/// segment of a shell command.
const DENIED_COMMANDS: &[(&str, &str)] = &[
    ("git push", "agents never push; Mestari pushes after review"),
    ("git remote", "agents don't change remotes"),
    ("git checkout", "agents stay on their branch"),
    ("git switch", "agents stay on their branch"),
    ("git config", "agents don't change git configuration"),
    ("git reset --hard", "agents don't throw work away"),
    ("git rebase", "agents don't rewrite history"),
    ("git filter-branch", "agents don't rewrite history"),
    ("gh", "agents don't talk to GitHub; Mestari does"),
    ("curl", "no network access from agents"),
    ("wget", "no network access from agents"),
    ("ssh", "no network access from agents"),
    ("scp", "no network access from agents"),
    ("nc", "no network access from agents"),
    ("sudo", "agents don't escalate privileges"),
    ("cargo publish", "agents don't publish crates"),
    ("cargo install", "agents don't install tools"),
    ("cargo login", "agents don't handle registry tokens"),
    ("npm publish", "agents don't publish packages"),
];

/// Places that hold credentials. Any tool touching them is blocked.
const SECRET_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gh",
    ".netrc",
    ".git-credentials",
    ".cargo/credentials",
    ".npmrc",
    ".docker/config.json",
    ".paja",
];

/// Decide one tool call. `Ok(None)` allows it, `Ok(Some(reason))` blocks it.
pub fn check(input: &HookInput) -> Result<Option<String>> {
    let cwd = normalize(&input.cwd);
    match input.tool_name.as_str() {
        "Bash" => {
            let command = input.tool_input["command"]
                .as_str()
                .context("a Bash call without a command")?;
            Ok(check_command(command, &cwd))
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let path = path_arg(&input.tool_input).context("a file edit without a path")?;
            Ok(check_path(&path, &cwd, true))
        }
        "Read" | "Glob" | "Grep" | "LS" => match path_arg(&input.tool_input) {
            Some(path) => Ok(check_path(&path, &cwd, false)),
            None => Ok(None),
        },
        "WebFetch" | "WebSearch" => Ok(Some("no network access from agents".into())),
        _ => Ok(None),
    }
}

fn path_arg(tool_input: &Value) -> Option<String> {
    ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| tool_input[*key].as_str())
        .map(str::to_string)
}

/// Writes stay inside the environment; reads may look outside it but never at secrets.
fn check_path(path: &str, cwd: &Path, write: bool) -> Option<String> {
    let full = normalize(&cwd.join(path));
    if touches_secret(&full.to_string_lossy()) {
        return Some(format!(
            "{} holds credentials; agents never touch it",
            full.display()
        ));
    }
    if write && !full.starts_with(cwd) {
        return Some(format!(
            "{} is outside this environment ({}); agents only change files in their own worktree",
            full.display(),
            cwd.display()
        ));
    }
    None
}

fn check_command(command: &str, cwd: &Path) -> Option<String> {
    for segment in segments(command) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        // Skip leading `VAR=value` assignments and wrappers like `env` or `command`.
        let start = words
            .iter()
            .position(|w| !w.contains('=') && !matches!(*w, "env" | "command" | "exec" | "nohup"))
            .unwrap_or(words.len());
        let words = &words[start..];
        let program = words.first().map(|w| w.rsplit('/').next().unwrap_or(w));
        for (denied, reason) in DENIED_COMMANDS {
            let parts: Vec<&str> = denied.split(' ').collect();
            if program == Some(parts[0])
                && parts[1..]
                    .iter()
                    .all(|part| words.iter().skip(1).any(|w| w == part))
            {
                return Some(format!("`{denied}` is not allowed: {reason}"));
            }
        }
        if segment.contains("PAJA_HOME") {
            return Some("agents don't change PAJA_HOME".into());
        }
        if touches_secret(&segment) {
            return Some("that command touches credentials; agents never do".into());
        }
        if program == Some("rm") {
            for target in words.iter().skip(1).filter(|w| !w.starts_with('-')) {
                let full = normalize(&cwd.join(target.trim_matches(|c| c == '"' || c == '\'')));
                if !full.starts_with(cwd) || full == cwd {
                    return Some(format!(
                        "`rm {target}` reaches outside this environment's files"
                    ));
                }
            }
        }
    }
    None
}

/// Split a shell command at `;`, `&&`, `||`, `|`, newlines, and `$(`/backtick substitutions,
/// so a denied command can't hide behind another one.
fn segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            ';' | '\n' | '|' | '&' | '`' | '(' | ')' => {
                out.push(std::mem::take(&mut current));
                if (c == '|' || c == '&') && chars.peek() == Some(&c) {
                    chars.next();
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                out.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    out.push(current);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn touches_secret(text: &str) -> bool {
    SECRET_PATHS.iter().any(|secret| {
        text.match_indices(secret).any(|(i, _)| {
            // A whole path segment: preceded by `/`, `~` or start, followed by `/`, end or space.
            let before = text[..i].chars().last();
            let after = text[i + secret.len()..].chars().next();
            matches!(
                before,
                None | Some('/') | Some('~') | Some(' ') | Some('"') | Some('\'')
            ) && matches!(after, None | Some('/') | Some(' ') | Some('"') | Some('\''))
        })
    })
}

/// Resolve `.` and `..` without touching the disk, so paths that don't exist yet still work.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// The input Mestari's self-test sends: a push, which must always be blocked.
pub fn self_test_input() -> String {
    serde_json::json!({
        "tool_name": "Bash",
        "tool_input": {"command": "git push origin main"},
        "cwd": "/"
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decide(tool: &str, input: Value) -> Option<String> {
        check(&HookInput {
            tool_name: tool.into(),
            tool_input: input,
            cwd: "/work/env".into(),
        })
        .unwrap()
    }

    fn bash(command: &str) -> Option<String> {
        decide("Bash", json!({ "command": command }))
    }

    #[test]
    fn ordinary_work_is_allowed() {
        for ok in [
            "cargo test",
            "cargo fmt --all --check",
            "git add -A && git commit -m 'fix'",
            "git status",
            "git diff main...HEAD",
            "rm -rf target/debug",
            "ls src",
            "echo github is fine as a word",
        ] {
            assert_eq!(bash(ok), None, "{ok}");
        }
        assert_eq!(
            decide("Write", json!({"file_path": "/work/env/src/x.rs"})),
            None
        );
        assert_eq!(decide("Edit", json!({"file_path": "src/x.rs"})), None);
        assert_eq!(
            decide("Read", json!({"file_path": "/usr/include/stdio.h"})),
            None
        );
    }

    #[test]
    fn reaching_outside_is_blocked_even_when_hidden() {
        for bad in [
            "git push origin main",
            "cargo test && git push",
            "echo $(curl https://example.com)",
            "cargo build; `wget x`",
            "FOO=1 gh pr merge 1",
            "/usr/bin/curl x",
            "env ssh host",
            "git -C . checkout main",
            "cat ~/.ssh/id_ed25519",
            "cat ~/.config/gh/hosts.yml",
            "PAJA_HOME=/real cargo run -- env list",
            "rm -rf /",
            "rm -rf ../other",
            "rm -rf .",
            "sudo ls",
            "cargo publish",
        ] {
            assert!(bash(bad).is_some(), "{bad} should be blocked");
        }
        assert!(decide("Write", json!({"file_path": "/etc/passwd"})).is_some());
        assert!(decide("Edit", json!({"file_path": "../../other/x"})).is_some());
        assert!(decide("Read", json!({"file_path": "/home/u/.ssh/id_rsa"})).is_some());
        assert!(decide("WebFetch", json!({"url": "https://example.com"})).is_some());
    }

    #[test]
    fn malformed_calls_are_errors() {
        let input = HookInput {
            tool_name: "Bash".into(),
            tool_input: json!({}),
            cwd: "/w".into(),
        };
        assert!(check(&input).is_err());
    }

    #[test]
    fn the_self_test_input_is_blocked() {
        let input: HookInput = serde_json::from_str(&self_test_input()).unwrap();
        assert!(check(&input).unwrap().is_some());
    }
}
