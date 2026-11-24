//! The deny-list hook: a Claude Code `PreToolUse` hook that every agent Mestari starts runs
//! under. It keeps an agent inside its Ahjo environment: no pushing or GitHub, no network, no
//! credentials, no factory state, and no changes to files outside its worktree.
//!
//! Mestari passes the environment's root and the factory's home explicitly
//! (`paja hook --root <env> --home <home>`), because the `cwd` Claude Code reports moves when
//! the agent runs `cd`. Shell commands are judged against an allowlist of programs, one simple
//! command at a time, because a shell has too many ways to hide a denied one.
//!
//! It is a guardrail, not a sandbox: code the agent writes still runs under `cargo test`.
//! Containing that is the sandbox's job.
//!
//! It fails closed. `paja hook` exits 2 (block) when a rule matches and also when anything goes
//! wrong while deciding, including a panic. Mestari runs a self-test before each agent and
//! refuses to start one unless the hook both blocks a push and allows a build.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

/// What Claude Code sends a `PreToolUse` hook on stdin.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub tool_name: String,
    #[serde(default)]
    pub tool_input: Value,
    /// Where the agent's shell is now. Used to resolve relative paths, never as the boundary.
    pub cwd: PathBuf,
}

/// The boundary Mestari sets for one agent.
#[derive(Debug, Clone)]
pub struct Bounds {
    /// The environment's worktree: the only place the agent may change files.
    pub root: PathBuf,
    /// The factory's state directory. Agents never touch it, apart from `root` inside it.
    pub home: PathBuf,
}

impl Bounds {
    /// Both paths must be absolute; Mestari always passes them.
    pub fn new(root: Option<PathBuf>, home: Option<PathBuf>) -> Result<Bounds> {
        match (root, home) {
            (Some(root), Some(home)) if root.is_absolute() && home.is_absolute() => Ok(Bounds {
                root: normalize(&root),
                home: normalize(&home),
            }),
            _ => bail!("`paja hook` needs absolute --root and --home paths; Mestari passes both"),
        }
    }
}

/// Programs an agent may run. Anything else is blocked, including shells and interpreters.
const PROGRAMS: &[&str] = &[
    "cargo", "rustc", "rustfmt", "git", "ls", "cat", "head", "tail", "wc", "grep", "rg", "find",
    "sort", "uniq", "diff", "echo", "printf", "pwd", "true", "false", "test", "[", "mkdir",
    "touch", "cp", "mv", "rm", "tr", "cut", "basename", "dirname", "which", "date", "mktemp", "cd",
    "sleep",
];

/// The git subcommands an agent may use: local work on its own branch.
const GIT_SUBCOMMANDS: &[&str] = &[
    "add",
    "commit",
    "status",
    "diff",
    "log",
    "show",
    "rev-parse",
    "ls-files",
    "blame",
    "grep",
    "mv",
    "rm",
    "restore",
    "stash",
];

/// Cargo subcommands that reach a registry or install tools.
const CARGO_DENIED: &[&str] = &["publish", "install", "login", "logout", "owner", "yank"];

/// Programs whose path arguments are changed, so each must stay inside the root.
const WRITERS: &[&str] = &["mkdir", "touch", "cp", "mv", "rm"];

/// Variables that change which program runs or what it loads.
const DENIED_VARIABLES: &[&str] = &["PATH", "BASH_ENV", "ENV", "RUSTC", "RUSTC_WRAPPER"];
const DENIED_VARIABLE_PREFIXES: &[&str] = &["LD_", "DYLD_", "GIT_"];

/// Places that hold credentials. Any tool touching them is blocked.
const SECRET_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gh",
    ".netrc",
    ".git-credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".npmrc",
    ".docker/config.json",
];

/// Decide one tool call. `Ok(None)` allows it, `Ok(Some(reason))` blocks it.
pub fn check(input: &HookInput, bounds: &Bounds) -> Result<Option<String>> {
    let cwd = normalize(&input.cwd);
    let t = &input.tool_input;
    match input.tool_name.as_str() {
        "Bash" => {
            let command = t["command"]
                .as_str()
                .context("a Bash call without a command")?;
            Ok(check_command(command, &cwd, bounds))
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let path = ["file_path", "notebook_path"]
                .iter()
                .find_map(|key| t[*key].as_str())
                .context("a file edit without a path")?;
            Ok(check_write(path, &cwd, bounds))
        }
        "Read" | "Glob" | "Grep" | "LS" => {
            for key in ["file_path", "path", "pattern", "glob"] {
                if let Some(value) = t[key].as_str() {
                    // A Grep pattern is a regex, not a path: only look for secrets in it.
                    let is_path = key != "pattern" || input.tool_name == "Glob";
                    if let Some(reason) = check_read(value, &cwd, bounds, is_path) {
                        return Ok(Some(reason));
                    }
                }
            }
            Ok(None)
        }
        "WebFetch" | "WebSearch" => Ok(Some("no network access from agents".into())),
        // Other tools (todo lists and the like) touch nothing outside the session.
        _ => Ok(None),
    }
}

/// Reads may look outside the worktree, but never at credentials or the factory's state.
fn check_read(value: &str, cwd: &Path, b: &Bounds, is_path: bool) -> Option<String> {
    if touches_secret(value) {
        return Some(format!("{value} holds credentials; agents never touch it"));
    }
    if is_path {
        let full = normalize(&cwd.join(value));
        if in_factory_home(&full, b) {
            return Some(format!(
                "{} is the factory's own state; agents never touch it",
                full.display()
            ));
        }
    }
    None
}

fn check_write(path: &str, cwd: &Path, b: &Bounds) -> Option<String> {
    if let Some(reason) = check_read(path, cwd, b, true) {
        return Some(reason);
    }
    let full = normalize(&cwd.join(path));
    if !full.starts_with(&b.root) {
        return Some(format!(
            "{} is outside this environment ({}); agents only change files in their own worktree",
            full.display(),
            b.root.display()
        ));
    }
    None
}

fn in_factory_home(path: &Path, b: &Bounds) -> bool {
    path.starts_with(&b.home) && !path.starts_with(&b.root)
}

fn check_command(command: &str, cwd: &Path, b: &Bounds) -> Option<String> {
    if touches_secret(command) {
        return Some("that command touches credentials; agents never do".into());
    }
    let home = b.home.to_string_lossy();
    if command.contains(home.as_ref()) || command.contains("~/.paja") {
        return Some("that command names the factory's own state; agents never touch it".into());
    }
    // A `cd` earlier in the command moves where later parts run, so track it.
    let mut here = cwd.to_path_buf();
    for segment in segments(command) {
        let words = words(&segment);
        let Some(start) = words.iter().position(|w| !is_assignment(w)) else {
            continue;
        };
        if let Some(var) = words[..start].iter().find_map(|w| denied_variable(w)) {
            return Some(format!(
                "agents don't set {var}; it changes which program runs"
            ));
        }
        let program = words[start].as_str();
        let args = &words[start + 1..];
        if !PROGRAMS.contains(&program) {
            return Some(format!(
                "`{program}` is not on the agents' allowlist; use the file tools, cargo and \
                 local git (run programs by name, not by path)"
            ));
        }
        if let Some(reason) = check_program(program, args, &here, b) {
            return Some(reason);
        }
        if let Some(reason) = check_redirects(&segment, &here, b) {
            return Some(reason);
        }
        if program == "cd" {
            here = normalize(&here.join(&args[0]));
        }
    }
    None
}

fn check_program(program: &str, args: &[String], here: &Path, b: &Bounds) -> Option<String> {
    // Paths the program will act on: plain operands and the values of `--opt=value`.
    let operands = || {
        args.iter().filter_map(|a| match a.strip_prefix("--") {
            Some(opt) => opt.split_once('=').map(|(_, value)| value),
            None => (!a.starts_with('-')).then_some(a.as_str()),
        })
    };
    for arg in operands() {
        if in_factory_home(&normalize(&here.join(arg)), b) {
            return Some(format!(
                "`{program} {arg}` reaches the factory's own state; agents never touch it"
            ));
        }
    }
    match program {
        "git" => {
            if let Some(opt) = args.iter().find(|a| {
                ["-C", "--git-dir", "--work-tree", "--namespace"]
                    .iter()
                    .any(|o| a.as_str() == *o || a.starts_with(&format!("{o}=")))
            }) {
                return Some(format!(
                    "`git {opt}` points git elsewhere; agents only use git in their own worktree"
                ));
            }
            let sub = git_subcommand(args);
            if !sub.is_some_and(|s| GIT_SUBCOMMANDS.contains(&s)) {
                return Some(format!(
                    "`git {}` is not allowed: agents only use local git on their own branch \
                     (add, commit, status, diff, log, show, stash)",
                    sub.unwrap_or("")
                ));
            }
        }
        "cargo" => {
            if let Some(sub) = args
                .iter()
                .find(|a| !a.starts_with('-') && !a.starts_with('+'))
                .filter(|s| CARGO_DENIED.contains(&s.as_str()))
            {
                return Some(format!(
                    "`cargo {sub}` is not allowed: agents don't publish or install"
                ));
            }
        }
        "find"
            if args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-exec"
                        | "-execdir"
                        | "-ok"
                        | "-okdir"
                        | "-delete"
                        | "-fprint"
                        | "-fprintf"
                        | "-fprint0"
                        | "-fls"
                )
            }) =>
        {
            return Some("`find` may only list files, not run, write or delete them".into());
        }
        "sort"
            if args
                .iter()
                .any(|a| a.starts_with("-o") || a.starts_with("--output")) =>
        {
            return Some("`sort` may only print; write files with the Write tool".into());
        }
        "cd" => match args.first() {
            Some(target) if !target.starts_with('-') => {
                return confined("cd", target, here, b);
            }
            _ => return Some("`cd` needs a plain directory inside the worktree".into()),
        },
        _ => {}
    }
    if WRITERS.contains(&program) {
        for target in operands() {
            if let Some(reason) = confined(program, target, here, b) {
                return Some(reason);
            }
        }
    }
    None
}

/// A path the shell will act on must stay inside the root. A path the shell would expand
/// first (`~`, `$`, backticks, globs) can't be judged here, so it is blocked.
fn confined(program: &str, target: &str, here: &Path, b: &Bounds) -> Option<String> {
    if target.contains(['~', '$', '`', '*', '?', '[']) {
        return Some(format!(
            "`{program} {target}` needs the shell to expand it; give a plain path inside the \
             worktree"
        ));
    }
    let full = normalize(&here.join(target));
    if !full.starts_with(&b.root) || (program == "rm" && full == b.root) {
        return Some(format!(
            "`{program} {target}` reaches outside this environment ({})",
            b.root.display()
        ));
    }
    None
}

/// Output redirections (`>`, `>>`, `2>`, `&>`) must write inside the root or to /dev/null.
fn check_redirects(segment: &str, here: &Path, b: &Bounds) -> Option<String> {
    let mut rest = segment;
    while let Some(i) = rest.find('>') {
        rest = &rest[i + 1..];
        let after = rest.trim_start_matches(['>', '&', '|']).trim_start();
        let target: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
        let target = target.trim_matches(['"', '\'']);
        if target.is_empty() || target == "/dev/null" || target.chars().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        if let Some(reason) = confined("redirect to", target, here, b) {
            return Some(reason);
        }
    }
    None
}

/// The subcommand of a git command line, skipping global options and their values.
fn git_subcommand(args: &[String]) -> Option<&str> {
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
        } else if matches!(arg.as_str(), "-c") {
            skip = true;
        } else if !arg.starts_with('-') {
            return Some(arg);
        }
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn denied_variable(assignment: &str) -> Option<&str> {
    let name = assignment.split_once('=')?.0;
    (DENIED_VARIABLES.contains(&name)
        || DENIED_VARIABLE_PREFIXES.iter().any(|p| name.starts_with(p)))
    .then_some(name)
}

/// Split a shell command into simple commands at `;`, `&&`, `||`, `|`, `&`, newlines,
/// subshells and substitutions, so each one is judged on its own. Quotes are kept for
/// [`words`]; single quotes are literal, and substitutions open even inside double quotes.
fn segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    // Substitutions being read: the closing character and the quote to return to.
    let mut open: Vec<(char, Option<char>)> = Vec::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if quote == Some('\'') {
            if c == '\'' {
                quote = None;
            }
            current.push(c);
            continue;
        }
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '$' if chars.peek() == Some(&'(') => {
                chars.next();
                open.push((')', quote));
                quote = None;
                out.push(std::mem::take(&mut current));
            }
            '`' if open.last().is_some_and(|(close, _)| *close == '`') && quote.is_none() => {
                quote = open.pop().and_then(|(_, q)| q);
                out.push(std::mem::take(&mut current));
            }
            '`' => {
                open.push(('`', quote));
                quote = None;
                out.push(std::mem::take(&mut current));
            }
            ')' if quote.is_none() && open.last().is_some_and(|(close, _)| *close == ')') => {
                quote = open.pop().and_then(|(_, q)| q);
                out.push(std::mem::take(&mut current));
            }
            '"' if quote == Some('"') => {
                quote = None;
                current.push(c);
            }
            '\'' | '"' if quote.is_none() => {
                quote = Some(c);
                current.push(c);
            }
            // `2>&1` and `&>` are redirections, not separators.
            '&' if quote.is_none() && (current.ends_with('>') || chars.peek() == Some(&'>')) => {
                current.push(c)
            }
            ';' | '\n' | '|' | '&' | '(' | ')' if quote.is_none() => {
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

/// The words of one simple command, with quotes removed.
fn words(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    for c in segment.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                quoted = true;
            }
            None if c.is_whitespace() => {
                if quoted || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                quoted = false;
            }
            None => current.push(c),
        }
    }
    if quoted || !current.is_empty() {
        out.push(current);
    }
    out
}

fn touches_secret(text: &str) -> bool {
    SECRET_PATHS.iter().any(|secret| {
        text.match_indices(secret).any(|(i, _)| {
            // A whole path segment: preceded by `/`, `~`, a quote, a space or the start, and
            // followed by `/`, a quote, a space, a glob or the end.
            let before = text[..i].chars().last();
            let after = text[i + secret.len()..].chars().next();
            matches!(before, None | Some('/' | '~' | ' ' | '"' | '\'' | '='))
                && matches!(after, None | Some('/' | ' ' | '"' | '\'' | '*'))
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

/// The two calls Mestari's self-test sends, run in `root`: a push that must be blocked and a
/// build that must be allowed.
pub fn self_test_inputs(root: &Path) -> (String, String) {
    let call = |command: &str| {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": root
        })
        .to_string()
    };
    (call("git push origin main"), call("cargo test"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOT: &str = "/h/.paja/envs/work-1";

    fn bounds() -> Bounds {
        Bounds::new(Some(ROOT.into()), Some("/h/.paja".into())).unwrap()
    }

    fn decide(tool: &str, input: Value) -> Option<String> {
        let input = HookInput {
            tool_name: tool.into(),
            tool_input: input,
            cwd: ROOT.into(),
        };
        check(&input, &bounds()).unwrap()
    }

    fn bash(command: &str) -> Option<String> {
        decide("Bash", json!({ "command": command }))
    }

    #[test]
    fn ordinary_work_is_allowed() {
        for ok in [
            "cargo test",
            "cargo fmt --all --check",
            "cargo clippy --all-targets -- -D warnings 2>&1 | tail -20",
            "git add -A && git commit -m 'push the fix (finally)'",
            "git commit -m \"fix: a & b; c | d (e)\"",
            "git status",
            "git stash push",
            "git diff main...HEAD",
            "git -c core.pager=cat log --oneline -3",
            "rm -rf target/debug",
            "ls src && cat Cargo.toml | head -5",
            "PAJA_HOME=$(mktemp -d) cargo run -q -- --json env create demo",
            "echo hi > notes.txt",
            "cargo test > /dev/null 2>&1",
            "cd crates/paja && cargo test",
            "grep -rn 'fn main' src",
        ] {
            assert_eq!(bash(ok), None, "{ok}");
        }
        assert_eq!(
            decide("Write", json!({"file_path": format!("{ROOT}/src/x.rs")})),
            None
        );
        assert_eq!(decide("Edit", json!({"file_path": "src/x.rs"})), None);
        assert_eq!(decide("Edit", json!({"file_path": ".paja/checks"})), None);
        assert_eq!(
            decide("Read", json!({"file_path": "/usr/include/stdio.h"})),
            None
        );
        assert_eq!(
            decide("Grep", json!({"pattern": "fn main", "path": "src"})),
            None
        );
    }

    #[test]
    fn reaching_outside_is_blocked_even_when_hidden() {
        for bad in [
            "git push origin main",
            "cargo test && git push",
            "\"git\" push",
            "g\\it push",
            "bash -c 'git push'",
            "sh -c 'curl x'",
            "eval git push",
            "env -i curl https://example.com",
            "echo $(curl https://example.com)",
            "echo \"$(curl https://example.com)\"",
            "cargo build; `wget x`",
            "FOO=1 gh pr merge 1",
            "/usr/bin/curl x",
            "./cargo test",
            "PATH=. cargo test",
            "GIT_DIR=/elsewhere git status",
            "git -C . checkout main",
            "git remote add x y",
            "git config --global user.name x",
            "git -c alias.x='!sh' x",
            "cat ~/.ssh/id_ed25519",
            "cat /h/.config/gh/hosts.yml",
            "cat /h/.paja/events.jsonl",
            "cat ../../events.jsonl",
            "ls ../work-2",
            "PAJA_HOME=/h/.paja cargo run -- env list",
            "rm -rf /",
            "rm -rf ~",
            "rm -rf $HOME",
            "rm -rf ../other",
            "rm -rf .",
            "rm -rf *",
            "cd .. && rm -rf work-1",
            "cd /tmp",
            "cd",
            "cd - && rm x",
            "echo x > /h/.bashrc",
            "echo x >> ../outside.txt",
            "echo x 2>~/log",
            "cp src/x.rs /tmp/x.rs",
            "find . -delete",
            "find . -exec rm {} ;",
            "sed -i s/a/b/ src/x.rs",
            "sed -n 'w /tmp/x' src/x.rs",
            "sort -o /tmp/x src/x.rs",
            "git -C /other/repo add -A",
            "git --work-tree=/elsewhere restore .",
            "cp --target-directory=/tmp/x src/a.rs",
            "mv --target-directory=/outside f",
            "cat --x=../../events.jsonl",
            "sudo ls",
            "python3 -c 'import os'",
            "xargs rm",
            "cargo publish",
            "cargo install ripgrep",
            "{ git push; }",
        ] {
            assert!(bash(bad).is_some(), "{bad} should be blocked");
        }
        assert!(decide("Write", json!({"file_path": "/etc/passwd"})).is_some());
        assert!(decide("Edit", json!({"file_path": "../../other/x"})).is_some());
        assert!(decide("Write", json!({"file_path": "/h/.paja/events.jsonl"})).is_some());
        assert!(decide("Read", json!({"file_path": "/h/.ssh/id_rsa"})).is_some());
        assert!(decide("Read", json!({"file_path": "/h/.paja/events.jsonl"})).is_some());
        assert!(decide("Glob", json!({"pattern": "/h/.ssh/**"})).is_some());
        assert!(decide("Glob", json!({"pattern": "../../*.jsonl"})).is_some());
        assert!(decide("Grep", json!({"pattern": ".", "glob": "/h/.aws/*"})).is_some());
        assert!(decide("WebFetch", json!({"url": "https://example.com"})).is_some());
    }

    #[test]
    fn the_boundary_is_the_root_not_where_the_shell_moved() {
        let input = HookInput {
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "src/x.rs"}),
            cwd: "/h/other-repo".into(),
        };
        assert!(check(&input, &bounds()).unwrap().is_some());
    }

    #[test]
    fn malformed_calls_are_errors() {
        let input = HookInput {
            tool_name: "Bash".into(),
            tool_input: json!({}),
            cwd: ROOT.into(),
        };
        assert!(check(&input, &bounds()).is_err());
        assert!(Bounds::new(Some("rel".into()), Some("/h".into())).is_err());
        assert!(Bounds::new(None, Some("/h".into())).is_err());
    }

    #[test]
    fn the_self_test_blocks_a_push_and_allows_a_build() {
        let (push, build) = self_test_inputs(Path::new(ROOT));
        let push: HookInput = serde_json::from_str(&push).unwrap();
        let build: HookInput = serde_json::from_str(&build).unwrap();
        assert!(check(&push, &bounds()).unwrap().is_some());
        assert!(check(&build, &bounds()).unwrap().is_none());
    }
}
