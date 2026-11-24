//! The deny-list hook: a Claude Code `PreToolUse` hook that every agent Mestari starts runs
//! under. It keeps an agent inside its Ahjo environment: no pushing or GitHub, no network, no
//! credentials, no factory state, and no changes to files outside its worktree.
//!
//! Mestari passes the environment's root and the factory's home explicitly
//! (`paja hook --root <env> --home <home>`), because the `cwd` Claude Code reports moves when
//! the agent runs `cd`.
//!
//! Shell commands are not parsed in general, since a shell has too many ways to hide a denied
//! command. Instead a command must have one of a few plain shapes: an optional `cd` into the
//! worktree, then `cargo`, local `git`, `ls`, `rm` or `pwd` commands joined by `&&`, each
//! optionally piped into `head`, `tail`, `grep` or `wc`. No expansions, no redirections other
//! than `2>&1`, and no other separators. Agents read and change files with the file tools.
//!
//! It is a guardrail, not a sandbox: code the agent writes still runs under `cargo test`.
//!
//! It fails closed. [`run`] exits 2 (block) when a rule matches and also when anything goes
//! wrong while deciding, including a panic. Mestari runs a self-test before each agent and
//! refuses to start one unless the hook both blocks a push and allows a build.

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

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
                root: resolve(&root),
                home: resolve(&home),
            }),
            _ => bail!("`paja hook` needs absolute --root and --home paths; Mestari passes both"),
        }
    }
}

/// `paja hook`: read one tool call on stdin and answer with an exit code Claude Code
/// understands. 0 allows; 2 blocks, with the reason on stderr. Anything but a clear "allow"
/// blocks, a panic included.
pub fn run(root: Option<PathBuf>, home: Option<PathBuf>) -> ExitCode {
    // A panic would exit 101, which Claude Code treats as "allow"; block instead.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Blocked: Paja's deny-list hook crashed ({info}), so it fails closed");
        std::process::exit(2);
    }));
    let decision = (|| -> Result<Option<String>> {
        let bounds = Bounds::new(root, home)?;
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let input: HookInput = serde_json::from_str(&input)?;
        check(&input, &bounds)
    })();
    match decision {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(reason)) => {
            eprintln!("Blocked by Paja's deny-list: {reason}");
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!(
                "Blocked: Paja's deny-list hook couldn't decide ({err:#}), so it fails closed"
            );
            ExitCode::from(2)
        }
    }
}

/// Cargo subcommands an agent may run. The subcommand must come first.
const CARGO_SUBCOMMANDS: &[&str] = &[
    "build", "check", "test", "fmt", "clippy", "run", "doc", "tree", "metadata",
];

/// Cargo options that load other configuration or write elsewhere.
const CARGO_DENIED_OPTIONS: &[&str] = &[
    "--config",
    "-Z",
    "--target-dir",
    "--manifest-path",
    "--artifact-dir",
    "--out-dir",
    "-C",
];

/// Git subcommands an agent may run: local work on its own branch. The subcommand must come
/// first, so no global option can point git elsewhere or set configuration.
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
    "rm",
    "mv",
    "restore",
];

/// Git options that run programs, read or write files outside the repository, or compare
/// arbitrary files.
const GIT_DENIED_OPTIONS: &[&str] = &[
    "--output",
    "--ext-diff",
    "--textconv",
    "-O",
    "--open-files-in-pager",
    "--exec",
    "-F",
    "--file",
    "-t",
    "--template",
    "--pathspec-from-file",
    "--no-index",
];

/// Git options whose value is a path.
const GIT_PATH_OPTIONS: &[&str] = &["--contents", "--ignore-revs-file", "--orderfile"];

/// Variables an agent may set in front of a command.
const ALLOWED_VARIABLES: &[&str] = &["RUST_BACKTRACE", "RUST_LOG"];

/// Programs that may read a pipe, with no file operands.
const PIPE_TARGETS: &[&str] = &["head", "tail", "grep", "wc"];

/// Places that hold credentials. Any tool naming them is blocked.
const SECRET_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".config/gh",
    ".netrc",
    ".git-credentials",
    ".cargo/credentials",
    ".npmrc",
    ".docker",
];

/// Hidden directories outside the worktree that are still worth reading: dependency
/// sources and the toolchain.
const READABLE_HIDDEN: &[&str] = &[".cargo/registry", ".cargo/git", ".rustup"];

/// Tools that touch nothing outside the session. Any tool not named here or handled in
/// [`check`], MCP tools included, is blocked.
const HARMLESS_TOOLS: &[&str] = &["TodoWrite", "TodoRead", "Task", "Agent", "ExitPlanMode"];

/// Decide one tool call. `Ok(None)` allows it, `Ok(Some(reason))` blocks it.
pub fn check(input: &HookInput, bounds: &Bounds) -> Result<Option<String>> {
    let cwd = resolve(&input.cwd);
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
            Ok(inside_root("change files", &cwd.join(path), bounds))
        }
        "Read" => {
            let path = t["file_path"].as_str().context("a Read without a path")?;
            Ok(check_read(&cwd.join(path), bounds))
        }
        // Searches recurse, so they stay inside the worktree.
        "Glob" | "Grep" | "LS" => {
            let base = t["path"].as_str().map_or(cwd.clone(), |p| cwd.join(p));
            if let Some(reason) = inside_root("search", &base, bounds) {
                return Ok(Some(reason));
            }
            // Grep's pattern is a regex; Glob's pattern and Grep's glob are paths.
            let key = if input.tool_name == "Grep" {
                "glob"
            } else {
                "pattern"
            };
            if let Some(pattern) = t[key].as_str() {
                if pattern.starts_with('/') || pattern.split('/').any(|part| part == "..") {
                    return Ok(Some(format!(
                        "the pattern `{pattern}` reaches outside the worktree; search relative to it"
                    )));
                }
            }
            Ok(None)
        }
        "WebFetch" | "WebSearch" => Ok(Some("no network access from agents".into())),
        name if HARMLESS_TOOLS.contains(&name) => Ok(None),
        name => Ok(Some(format!(
            "agents don't use the `{name}` tool; Mestari's agents use the file tools, cargo and \
             local git"
        ))),
    }
}

/// `path` must be inside the worktree, and not git's link to the repository.
fn inside_root(what: &str, path: &Path, b: &Bounds) -> Option<String> {
    let full = resolve(path);
    if !full.starts_with(&b.root) {
        return Some(format!(
            "agents {what} only inside their own worktree ({}), not {}",
            b.root.display(),
            full.display()
        ));
    }
    if full.starts_with(b.root.join(".git")) {
        return Some(format!(
            "{} is git's own link to the repository; agents never change it",
            full.display()
        ));
    }
    if full.starts_with(b.root.join(".cargo")) {
        return Some(format!(
            "{} is cargo configuration, which could change how cargo runs; agents never change it",
            full.display()
        ));
    }
    None
}

/// Reads may look at plain files outside the worktree, but not into hidden directories
/// (where credentials live) other than dependency sources, and never at the factory's state.
fn check_read(path: &Path, b: &Bounds) -> Option<String> {
    let full = resolve(path);
    if full.starts_with(&b.root) {
        return None;
    }
    if ["/proc", "/sys", "/dev"]
        .iter()
        .any(|p| full.starts_with(p))
    {
        return Some(format!(
            "{} exposes processes and devices; agents don't read it",
            full.display()
        ));
    }
    if full.starts_with(&b.home) {
        return Some(format!(
            "{} is the factory's own state; agents never touch it",
            full.display()
        ));
    }
    let text = full.to_string_lossy();
    // Every hidden directory on the way must be one of the dependency roots.
    let parts: Vec<String> = full
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let hidden_ok = parts.iter().enumerate().all(|(n, part)| {
        !part.starts_with('.')
            || READABLE_HIDDEN.iter().any(|dir| {
                let mut want = dir.split('/');
                want.next() == Some(part.as_str())
                    && want.all(|next| parts.get(n + 1).map(String::as_str) == Some(next))
            })
    });
    if touches_secret(&text) || !hidden_ok {
        return Some(format!(
            "{} is in a hidden directory outside the worktree; agents don't read those",
            full.display()
        ));
    }
    None
}

/// One piece of a command line.
#[derive(Debug, PartialEq)]
enum Token {
    Word(String),
    And,
    Pipe,
    StderrToStdout,
}

/// Split a command into words and the few operators allowed. Anything the shell would
/// expand or treat specially, outside single quotes, is an error.
fn tokenize(command: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars().peekable();
    let flush = |word: &mut String, in_word: &mut bool, tokens: &mut Vec<Token>| {
        if *in_word {
            tokens.push(Token::Word(std::mem::take(word)));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("an unclosed quote".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some(c @ ('$' | '`' | '\\' | '!')) => {
                            return Err(format!("`{c}` inside double quotes (use single quotes)"));
                        }
                        Some(c) => word.push(c),
                        None => return Err("an unclosed quote".into()),
                    }
                }
            }
            ' ' | '\t' => flush(&mut word, &mut in_word, &mut tokens),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                flush(&mut word, &mut in_word, &mut tokens);
                tokens.push(Token::And);
            }
            '|' if chars.peek() != Some(&'|') => {
                flush(&mut word, &mut in_word, &mut tokens);
                tokens.push(Token::Pipe);
            }
            '2' if !in_word && next_is(&mut chars, ">&1") => {
                tokens.push(Token::StderrToStdout);
            }
            c if c.is_ascii_alphanumeric() || "-_./=:,+@%^".contains(c) => {
                in_word = true;
                word.push(c);
            }
            '\n' => return Err("a newline".into()),
            c => return Err(format!("`{c}` outside quotes")),
        }
    }
    flush(&mut word, &mut in_word, &mut tokens);
    Ok(tokens)
}

/// Consume `rest` from `chars` if it comes next and ends the word.
fn next_is(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, rest: &str) -> bool {
    let ahead: String = chars.clone().take(rest.len() + 1).collect();
    let ends = ahead.len() == rest.len() || ahead.ends_with([' ', '\t', '|', '&']);
    if ahead.starts_with(rest) && ends {
        for _ in 0..rest.len() {
            chars.next();
        }
        true
    } else {
        false
    }
}

const SHAPE: &str = "agents run `cargo`, local `git`, `ls`, `rm` and `pwd`, joined by `&&` and \
     optionally piped into `head`, `tail`, `grep` or `wc`; use the file tools for everything else";

fn check_command(command: &str, cwd: &Path, b: &Bounds) -> Option<String> {
    if touches_secret(command) {
        return Some("that command names credentials; agents never touch them".into());
    }
    let tokens = match tokenize(command) {
        Ok(tokens) => tokens,
        Err(what) => return Some(format!("the command has {what}; {SHAPE}")),
    };
    // `&&` steps, each a pipeline of simple commands.
    let mut here = cwd.to_path_buf();
    for (i, step) in tokens.split(|t| *t == Token::And).enumerate() {
        let stages: Vec<&[Token]> = step.split(|t| *t == Token::Pipe).collect();
        // A `cd` only moves the shell when it is the whole first step.
        let moves = i == 0 && stages.len() == 1;
        for (j, stage) in stages.into_iter().enumerate() {
            let mut words: Vec<&str> = Vec::new();
            for (k, token) in stage.iter().enumerate() {
                match token {
                    Token::Word(w) => words.push(w),
                    Token::StderrToStdout if k == stage.len() - 1 => {}
                    _ => return Some(format!("a misplaced operator; {SHAPE}")),
                }
            }
            let reason = if j == 0 {
                check_simple(&words, moves, &mut here, b)
            } else {
                check_pipe_target(&words)
            };
            if reason.is_some() {
                return reason;
            }
        }
    }
    None
}

fn check_simple(words: &[&str], first: bool, here: &mut PathBuf, b: &Bounds) -> Option<String> {
    let start = words
        .iter()
        .position(|w| !is_assignment(w))
        .unwrap_or(words.len());
    for assignment in &words[..start] {
        let name = assignment.split_once('=').map_or("", |(n, _)| n);
        if !ALLOWED_VARIABLES.contains(&name) {
            return Some(format!(
                "agents don't set {name} (PAJA_HOME is already a throwaway home); {SHAPE}"
            ));
        }
    }
    let Some((program, args)) = words[start..].split_first() else {
        return Some(format!("an empty command; {SHAPE}"));
    };
    match *program {
        "cargo" => {
            let sub = args.first().copied().unwrap_or("");
            if !CARGO_SUBCOMMANDS.contains(&sub) {
                return Some(format!(
                    "`cargo {sub}` is not allowed; agents use cargo {}",
                    CARGO_SUBCOMMANDS.join(", ")
                ));
            }
            let own: Vec<&str> = args.iter().take_while(|a| **a != "--").copied().collect();
            if let Some(opt) = denied_option(&own, CARGO_DENIED_OPTIONS) {
                return Some(format!("`cargo {sub} {opt}` is not allowed for agents"));
            }
        }
        "git" => {
            let sub = args.first().copied().unwrap_or("");
            if !GIT_SUBCOMMANDS.contains(&sub) {
                return Some(format!(
                    "`git {sub}` is not allowed: agents only use local git on their own branch \
                     ({})",
                    GIT_SUBCOMMANDS.join(", ")
                ));
            }
            // A commit message is not an option or a path; leave it out of both checks.
            let takes_message = sub == "commit";
            let mut rest = Vec::new();
            let mut message = false;
            for arg in &args[1..] {
                if std::mem::take(&mut message) {
                    continue;
                }
                if takes_message && matches!(*arg, "-m" | "--message") {
                    message = true;
                } else if !(takes_message
                    && (arg.starts_with("-m") || arg.starts_with("--message=")))
                {
                    rest.push(*arg);
                }
            }
            if let Some(opt) = denied_option(&rest, GIT_DENIED_OPTIONS) {
                return Some(format!("`git {sub} {opt}` is not allowed for agents"));
            }
            // Every path git is given, as an operand or the value of an option that takes a
            // path, stays in the worktree. `rev:path` names a file in a revision.
            let paths: Vec<&str> = rest
                .iter()
                .filter_map(|arg| match arg.split_once('=') {
                    Some((opt, value)) if GIT_PATH_OPTIONS.contains(&opt) => Some(value),
                    _ if GIT_PATH_OPTIONS.contains(arg) => None,
                    _ => (!arg.starts_with('-') && !is_revision_path(arg)).then_some(*arg),
                })
                .collect();
            if let Some(opt) = rest.iter().find(|a| GIT_PATH_OPTIONS.contains(a)) {
                return Some(format!(
                    "write `{opt}=<path>` so the path can be checked against the worktree"
                ));
            }
            if let Some(reason) = operands_inside(&paths, here, b, "use git") {
                return Some(reason);
            }
        }
        "ls" | "rm" => {
            if let Some(reason) = operands_inside(args, here, b, "use ls and rm") {
                return Some(reason);
            }
            if *program == "rm"
                && args
                    .iter()
                    .any(|a| !a.starts_with('-') && resolve(&here.join(a)) == b.root)
            {
                return Some("agents don't remove their whole worktree".into());
            }
        }
        "pwd" => {}
        // Only as the first step, so a failed `cd` stops the rest.
        // A bare relative name could follow CDPATH and `-` goes back to OLDPWD, so neither
        // is allowed.
        "cd" if first => {
            let [target] = args else {
                return Some("`cd` takes one directory inside the worktree".into());
            };
            if !(target.starts_with('/') || target.starts_with("./") || target.starts_with("../")) {
                return Some(format!(
                    "`cd {target}`: give an absolute path or one starting with `./`"
                ));
            }
            let to = resolve(&here.join(target));
            if let Some(reason) = inside_root("cd", &to, b) {
                return Some(reason);
            }
            *here = to;
        }
        "cd" => return Some("`cd` may only be the first step of a command, on its own".into()),
        other => return Some(format!("`{other}` is not allowed; {SHAPE}")),
    }
    None
}

fn check_pipe_target(words: &[&str]) -> Option<String> {
    let Some((program, args)) = words.split_first() else {
        return Some(format!("an empty pipe; {SHAPE}"));
    };
    if !PIPE_TARGETS.contains(program) {
        return Some(format!("`{program}` can't read a pipe here; {SHAPE}"));
    }
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    // Nothing that names a file: grep takes flags and one pattern; head and tail take a
    // count (`-20`, `-n20`, `-n 20`); wc takes `-l`, `-c` or `-w`.
    let ok = match *program {
        "grep" => {
            args.iter().filter(|a| !a.starts_with('-')).count() == 1
                && args
                    .iter()
                    .filter(|a| a.starts_with('-'))
                    .all(|a| a.len() > 1 && a[1..].chars().all(|c| "inEvcwFxqo".contains(c)))
        }
        "wc" => args.iter().all(|a| matches!(*a, "-l" | "-c" | "-w")),
        _ => {
            let mut count_next = false;
            args.iter().all(|a| {
                if std::mem::take(&mut count_next) {
                    return digits(a);
                }
                match a.strip_prefix('-') {
                    Some("n" | "c") => {
                        count_next = true;
                        true
                    }
                    Some(rest) => digits(rest.strip_prefix(['n', 'c']).unwrap_or(rest)),
                    None => false,
                }
            }) && !count_next
        }
    };
    (!ok).then(|| format!("`{program}` may only filter the pipe, not read files; {SHAPE}"))
}

/// The first argument that is one of `denied`, sets it with `=`, or (for a short option)
/// has its value attached.
fn denied_option<'a>(args: &[&'a str], denied: &[&str]) -> Option<&'a str> {
    args.iter().copied().find(|arg| {
        denied.iter().any(|opt| {
            *arg == *opt
                || arg.starts_with(&format!("{opt}="))
                || (opt.len() == 2 && arg.starts_with(opt))
        })
    })
}

/// `HEAD:src/lib.rs` or `main:x` names a file in a revision, not on disk.
fn is_revision_path(arg: &str) -> bool {
    arg.split_once(':')
        .is_some_and(|(rev, _)| !rev.is_empty() && !rev.contains('/'))
}

fn operands_inside(args: &[&str], here: &Path, b: &Bounds, what: &str) -> Option<String> {
    for arg in args.iter().filter(|a| !a.starts_with('-')) {
        if let Some(reason) = inside_root(what, &here.join(arg), b) {
            return Some(reason);
        }
    }
    None
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn touches_secret(text: &str) -> bool {
    SECRET_PATHS.iter().any(|secret| text.contains(secret))
}

/// Resolve symlinks in the part of `path` that exists, and `.` and `..` everywhere, so a link
/// can't lead out of the worktree and paths that don't exist yet still work.
fn resolve(path: &Path) -> PathBuf {
    // Walk one component at a time, as the kernel does: a `..` after a symlink leaves the
    // link's target, not the link.
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
                out.pop();
            }
            other => {
                out.push(other);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                }
            }
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
            "cargo test -q 2>&1 | grep -E 'test result|FAILED'",
            "git add -A && git commit -m 'push the fix (finally); a & b | c $x'",
            "git commit -m \"fix: a & b; c | d (e)\"",
            "git commit -m '/etc parsing'",
            "git status",
            "git commit -m '-Fix parsing'",
            "git log --grep=/api --author=/x/",
            "git diff main...HEAD",
            "git log --oneline -3 | head -3",
            "git show HEAD:crates/paja/src/lib.rs",
            "rm -rf target/debug",
            "ls src",
            "pwd",
            "RUST_BACKTRACE=1 cargo test smoke",
            "cargo run -q -- --json env create demo",
            "cd /h/.paja/envs/work-1 && cargo test",
            "cd ./crates/paja && cargo test",
            "cargo test 2>&1|tail -n 20",
            "git log -m --oneline -3",
            "git log --format=%H -1",
            "cargo test | wc -l",
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
            decide(
                "Read",
                json!({"file_path": "/h/.cargo/registry/src/x/serde-1.0/src/lib.rs"})
            ),
            None
        );
        assert_eq!(
            decide("Grep", json!({"pattern": "fn main", "path": "src"})),
            None
        );
        assert_eq!(decide("Glob", json!({"pattern": "**/*.rs"})), None);
    }

    #[test]
    fn reaching_outside_is_blocked_even_when_hidden() {
        for bad in [
            "git push origin main",
            "cargo test && git push",
            "\"git\" push",
            "g\\it push",
            "bash -c 'git push'",
            "eval git push",
            "env -i curl https://example.com",
            "echo $(curl https://example.com)",
            "git commit -m \"$(curl x)\"",
            "cargo build; `wget x`",
            "cargo test || git push",
            "cargo test & git push",
            "cargo test\ngit push",
            "gh pr merge 1",
            "/usr/bin/git status",
            "./cargo test",
            "PATH=. cargo test",
            "PAJA_HOME=$(mktemp -d) cargo run -q -- env list",
            "PAJA_HOME=/h/.paja cargo run -q -- env list",
            "GIT_DIR=/elsewhere git status",
            "git -C /other/repo add -A",
            "git -c core.editor=sh commit",
            "git --work-tree=/elsewhere restore .",
            "git remote add x y",
            "git checkout main",
            "git config user.name x",
            "git diff --output=/h/.profile",
            "git diff --ext-diff",
            "git grep -O sh x",
            "git grep -Osh x",
            "git commit -F /etc/passwd",
            "git diff --no-index /h/a /h/b",
            "git add ../work-2/x",
            "cargo --color always publish",
            "cargo publish",
            "cargo install ripgrep",
            "cargo build --config build.rustc-wrapper=sh",
            "cargo build --target-dir /tmp/x",
            "cat /etc/passwd",
            "cat ~/.ssh/id_ed25519",
            "ls ~",
            "ls /h",
            "ls ../work-2",
            "rm -rf /",
            "rm -rf ../other",
            "rm -rf .",
            "rm -rf *",
            "rm .git",
            "cd .. && rm -rf work-2",
            "cd nope || rm -rf ../work-2",
            "cargo test && cd .. && rm -rf x",
            "cd /tmp",
            "echo x > notes.txt",
            "cargo test > /dev/null",
            "cargo test | sh",
            "cargo test | grep x /h/.paja/events.jsonl",
            "cargo test | tail -f /etc/passwd",
            "rg --pre sh . x.sh",
            "sort -o /tmp/x x",
            "find . -delete",
            "sed -n p x",
            "python3 -c 'import os'",
            "{ git push; }",
            "cd - && rm -rf src",
            "cd src && rm -rf x",
            "git blame --contents=/h/.paja/events.jsonl src/lib.rs",
            "git diff -m /h/.paja/events.jsonl src/lib.rs",
            "rm -rf ../work-2/a:b",
            "ls /h/.paja/envs/work-2:",
            "cargo test | head 10",
            "cargo test | tail -n",
            "cd ./a/b | wc -l && rm -rf ../work-2",
            "git stash clear",
            "git stash pop",
            "git blame --contents /h/.paja/events.jsonl src/lib.rs",
        ] {
            assert!(bash(bad).is_some(), "{bad} should be blocked");
        }
        assert!(decide("Write", json!({"file_path": "/etc/passwd"})).is_some());
        assert!(decide("Edit", json!({"file_path": "../../other/x"})).is_some());
        assert!(decide("Write", json!({"file_path": ".git"})).is_some());
        assert!(decide("Write", json!({"file_path": "/h/.paja/events.jsonl"})).is_some());
        assert!(decide("Read", json!({"file_path": "/h/.ssh/id_rsa"})).is_some());
        assert!(decide("Read", json!({"file_path": "/h/.config/gh/hosts.yml"})).is_some());
        assert!(decide("Read", json!({"file_path": "/h/.paja/events.jsonl"})).is_some());
        assert!(decide("Read", json!({"file_path": "../../events.jsonl"})).is_some());
        assert!(decide("Glob", json!({"pattern": "/h/.ssh/**"})).is_some());
        assert!(decide("Glob", json!({"pattern": "../../*.jsonl"})).is_some());
        assert!(decide("Grep", json!({"pattern": "BEGIN", "path": "/h"})).is_some());
        assert!(decide("Grep", json!({"pattern": ".", "glob": "/h/.aws/*"})).is_some());
        assert!(decide("Write", json!({"file_path": ".cargo/config.toml"})).is_some());
        assert!(decide("Read", json!({"file_path": "/proc/self/environ"})).is_some());
        assert!(decide(
            "Read",
            json!({"file_path": "/h/.config/app/.cargo/registry/token"})
        )
        .is_some());
        assert!(decide("WebFetch", json!({"url": "https://example.com"})).is_some());
        assert!(decide("mcp__github__merge_pull_request", json!({})).is_some());
        assert_eq!(decide("TodoWrite", json!({"todos": []})), None);
    }

    #[test]
    fn symlinks_cannot_lead_out_of_the_worktree() {
        let tmp = std::env::temp_dir().join(format!("paja-hook-{}", std::process::id()));
        let root = tmp.join("home/envs/a");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let b = Bounds::new(Some(root.clone()), Some(tmp.join("home"))).unwrap();
        let call = |tool: &str, path: &str| {
            let input = HookInput {
                tool_name: tool.into(),
                tool_input: json!({ "file_path": path }),
                cwd: root.clone(),
            };
            check(&input, &b).unwrap()
        };
        assert!(call("Write", "link/x").is_some());
        // `link/..` is the link target's parent, outside the worktree.
        assert!(call("Write", "link/../x").is_some());
        assert_eq!(call("Write", "src/new/x.rs"), None);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn the_boundary_is_the_root_not_where_the_shell_moved() {
        let input = HookInput {
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "src/x.rs"}),
            cwd: "/h/other-repo".into(),
        };
        assert!(check(&input, &bounds()).unwrap().is_some());
        // A search with no path searches the shell's cwd, which must be in the worktree.
        let input = HookInput {
            tool_name: "Grep".into(),
            tool_input: json!({"pattern": "x"}),
            cwd: "/h".into(),
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
