//! The deny-list hook: a Claude Code `PreToolUse` hook that every agent Mestari starts runs
//! under. It keeps an agent inside its Ahjo environment: no pushing or GitHub, no network, no
//! credentials, no factory state, and no changes to files outside its worktree.
//!
//! Mestari passes the environment's root and the factory's home explicitly
//! (`paja hook --root <env> --home <home>`), because the `cwd` Claude Code reports moves when
//! the agent runs `cd`.
//!
//! Shell commands are not parsed in general, since a shell has too many ways to hide a denied
//! command. Instead a command must have one plain shape: an optional `cd` to the worktree's
//! root, then `cargo` and local `git` commands joined by `&&`, each optionally piped into
//! `head`, `tail`, `grep` or `wc`. Every flag must be one Paja lists for that subcommand. No
//! expansions, no redirections other than `2>&1`, and no other separators. Agents read and
//! change files with the file tools.
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

/// How long the hook may take before it blocks. Mestari gives Claude Code a longer timeout.
pub const HOOK_DEADLINE_SECS: u64 = 20;

/// `paja hook`: read one tool call on stdin and answer with an exit code Claude Code
/// understands. 0 allows; 2 blocks, with the reason on stderr. Anything but a clear "allow"
/// blocks, a panic included.
pub fn run(root: Option<PathBuf>, home: Option<PathBuf>) -> ExitCode {
    // A hook that runs past Claude Code's timeout counts as "allow"; block well before it.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(HOOK_DEADLINE_SECS));
        eprintln!("Blocked: Paja's deny-list hook took too long to decide, so it fails closed");
        std::process::exit(2);
    });
    // A panic would exit 101, which Claude Code treats as "allow"; block instead.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Blocked: Paja's deny-list hook crashed ({info}), so it fails closed");
        std::process::exit(2);
    }));
    let decision = (|| -> Result<Option<String>> {
        // Read the call first, so the caller's write never meets a closed pipe.
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let bounds = Bounds::new(root, home)?;
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
                "Blocked: Paja's deny-list hook couldn't decide ({err:#}), so it fails closed. \
                 This is a hook setup problem, not the agent's: rebuild paja from main and \
                 restart `paja run`"
            );
            ExitCode::from(2)
        }
    }
}

/// Cargo subcommands an agent may run, and the flags each accepts. The subcommand comes
/// first, and every word must be one Paja knows, so no option can load other configuration,
/// run another program or write elsewhere.
const CARGO_SUBCOMMANDS: &[&str] = &["build", "check", "test", "fmt", "clippy", "run", "doc"];

/// Cargo flags that take no value.
const CARGO_FLAGS: &[&str] = &[
    "-q",
    "--quiet",
    "-v",
    "--verbose",
    "--all",
    "--workspace",
    "--all-targets",
    "--all-features",
    "--lib",
    "--bins",
    "--tests",
    "--benches",
    "--examples",
    "--doc",
    "--release",
    "--locked",
    "--offline",
    "--frozen",
    "--no-fail-fast",
    "--no-deps",
    "--check",
];

/// Cargo flags whose value is a package, target, feature or job count.
const CARGO_NAME_FLAGS: &[&str] = &[
    "-p",
    "--package",
    "--test",
    "--bin",
    "--example",
    "--features",
    "-j",
    "--jobs",
];

/// Flags the test harness accepts after `--`.
const TEST_HARNESS_FLAGS: &[&str] = &[
    "--nocapture",
    "--exact",
    "--ignored",
    "--include-ignored",
    "--show-output",
    "-q",
    "--quiet",
];

/// Git subcommands an agent may run, local work on its own branch, and the flags each
/// accepts. The subcommand comes first, so no global option can point git elsewhere or set
/// configuration; `stash` is out because its refs are shared with every checkout.
const GIT_FLAGS: &[(&str, &[&str])] = &[
    ("add", &["-A", "--all", "-u", "--update", "-v"]),
    ("commit", &["-a", "--all", "-q", "--quiet"]),
    (
        "status",
        &["-s", "--short", "--porcelain", "-b", "--branch"],
    ),
    ("diff", DIFF_FLAGS),
    ("show", DIFF_FLAGS),
    ("log", DIFF_FLAGS),
    ("rm", &["-r", "-q", "--cached", "-f"]),
];

const DIFF_FLAGS: &[&str] = &[
    "--stat",
    "--shortstat",
    "--name-only",
    "--name-status",
    "--oneline",
    "--cached",
    "--staged",
    "-p",
    "--patch",
    "--no-patch",
    "--no-color",
    "-n",
];

/// Words that make `cargo run -- ...` change the repository or reach GitHub and Claude.
const RUN_DENIED: &[&str] = &["create", "remove", "sync", "claim", "set", "run", "mcp"];

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
    // File tools expand `~` themselves, so a path starting with it can't be judged here.
    for key in ["file_path", "notebook_path", "path", "pattern", "glob"] {
        if input.tool_name == "Bash" || (input.tool_name == "Grep" && key == "pattern") {
            continue;
        }
        if let Some(path) = t[key].as_str() {
            if path.starts_with('~') {
                return Ok(Some(format!(
                    "`{path}` starts with `~`; give an absolute path or one inside the worktree"
                )));
            }
            if touches_secret(path) {
                return Ok(Some(format!(
                    "`{path}` names a credential path; agents never touch those"
                )));
            }
        }
    }
    match input.tool_name.as_str() {
        "Bash" => {
            let command = t["command"]
                .as_str()
                .context("a Bash call without a command")?;
            if t["run_in_background"] == Value::Bool(true) {
                return Ok(Some(
                    "agents run commands in the foreground, so their output comes back".into(),
                ));
            }
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
                if pattern.starts_with('/')
                    || pattern.contains("..")
                    || pattern.contains(['{', '}'])
                {
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
    // Git and cargo read configuration from these at any depth.
    let inner = full.strip_prefix(&b.root).unwrap_or(&full);
    // Compared without case, for case-insensitive filesystems such as macOS's default.
    let named = |name: &str| {
        inner
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(name))
    };
    if named(".git") {
        return Some(format!(
            "{} is git's own data or link to the repository; agents never change it",
            full.display()
        ));
    }
    if named(".cargo") {
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
    // Outside the worktree, only the toolchain and dependency sources.
    let dependency = parts
        .windows(2)
        .any(|w| w[0] == ".cargo" && (w[1] == "registry" || w[1] == "git"))
        || parts.iter().any(|p| p == ".rustup");
    if touches_secret(&text) || !hidden_ok || !(dependency || full.starts_with("/usr")) {
        return Some(format!(
            "{} is outside the worktree; agents read only the worktree, /usr and dependency \
             sources (~/.cargo/registry, ~/.cargo/git, ~/.rustup)",
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

const SHAPE: &str = "agents run `cargo` and local `git` with plain flags, joined by `&&` and \
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
        "cargo" => check_cargo(args),
        "git" => check_git(args, here, b),
        "pwd" if args.is_empty() => None,
        // Only into the worktree's root, and only as a step of its own, so a failed `cd`
        // stops the rest.
        "cd" if first => match args {
            [target] if target.starts_with('/') && resolve(Path::new(target)) == b.root => {
                *here = b.root.clone();
                None
            }
            _ => Some(format!(
                "`cd` only goes to the worktree's root, {}",
                b.root.display()
            )),
        },
        "cd" => Some("`cd` may only be the first step of a command, on its own".into()),
        other => Some(format!("`{other}` is not allowed; {SHAPE}")),
    }
}

fn check_cargo(args: &[&str]) -> Option<String> {
    let sub = args.first().copied().unwrap_or("");
    if !CARGO_SUBCOMMANDS.contains(&sub) {
        return Some(format!(
            "`cargo {sub}` is not allowed; agents use cargo {}",
            CARGO_SUBCOMMANDS.join(", ")
        ));
    }
    let rest = &args[1..];
    let split = rest.iter().position(|a| *a == "--").unwrap_or(rest.len());
    let (own, after) = (&rest[..split], rest.get(split + 1..).unwrap_or(&[]));
    let mut value_next = false;
    for arg in own {
        let ok = if std::mem::take(&mut value_next) {
            is_name(arg)
        } else if CARGO_FLAGS.contains(arg) {
            true
        } else if CARGO_NAME_FLAGS.contains(arg) {
            value_next = true;
            true
        } else if let Some((flag, value)) = arg.split_once('=') {
            CARGO_NAME_FLAGS.contains(&flag) && is_name(value)
        } else {
            // A test name filter, or a binary's arguments for `cargo run`.
            // A test name filter. `cargo run` takes the binary's arguments only after `--`.
            sub == "test" && is_name(arg)
        };
        if !ok {
            return Some(format!(
                "`cargo {sub} {arg}` is not allowed: agents use plain cargo flags ({})",
                CARGO_FLAGS.join(" ")
            ));
        }
    }
    if value_next {
        return Some(format!("`cargo {sub}`: a flag is missing its value"));
    }
    // After `--`, the words go to the program cargo starts.
    let mut lint_next = false;
    for arg in after {
        let ok = match sub {
            "test" => {
                TEST_HARNESS_FLAGS.contains(arg)
                    || arg
                        .strip_prefix("--test-threads=")
                        .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
                    || is_name(arg)
            }
            "clippy" if std::mem::take(&mut lint_next) => is_name(arg),
            "clippy" => match arg.get(..2) {
                Some("-D" | "-W" | "-A") if arg.len() == 2 => {
                    lint_next = true;
                    true
                }
                Some("-D" | "-W" | "-A") => is_name(&arg[2..]),
                _ => false,
            },
            "fmt" => *arg == "--check",
            // `cargo run -- --json work list`: the worktree's own binary, with plain words.
            // Commands that change the repository or reach GitHub are left to the smoke tests.
            "run" if RUN_DENIED.contains(arg) => false,
            "run" => is_name(arg) || arg.starts_with("--") && is_name(&arg[2..]),
            _ => false,
        };
        if !ok {
            return Some(format!(
                "`cargo {sub} -- {arg}` is not allowed for agents; pass plain names and flags"
            ));
        }
    }
    None
}

fn check_git(args: &[&str], here: &Path, b: &Bounds) -> Option<String> {
    let sub = args.first().copied().unwrap_or("");
    let Some((_, flags)) = GIT_FLAGS.iter().find(|(name, _)| *name == sub) else {
        let names: Vec<&str> = GIT_FLAGS.iter().map(|(name, _)| *name).collect();
        return Some(format!(
            "`git {sub}` is not allowed: agents only use local git on their own branch ({})",
            names.join(", ")
        ));
    };
    let mut paths_only = false;
    let mut skip_next = false;
    for arg in &args[1..] {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if !paths_only && sub == "commit" && matches!(*arg, "-m" | "--message") {
            // A commit message is free text, neither a flag nor a path.
            skip_next = true;
        } else if !paths_only && *arg == "--" {
            paths_only = true;
        } else if !paths_only && arg.starts_with('-') {
            let count = arg[1..].chars().all(|c| c.is_ascii_digit());
            if !flags.contains(arg) && !(count && DIFF_FLAGS == *flags) {
                return Some(format!(
                    "`git {sub} {arg}` is not allowed: agents use {}",
                    flags.join(" ")
                ));
            }
        } else if !is_plain(arg) {
            return Some(format!(
                "`git {sub} {arg}`: name revisions and paths plainly"
            ));
        } else if paths_only || !is_revision_path(arg) {
            // A revision such as `main...HEAD` stays inside the root as a path too; anything
            // that leaves it is a path outside the worktree.
            if let Some(reason) = inside_root("use git", &here.join(arg), b) {
                return Some(reason);
            }
        }
    }
    if skip_next {
        return Some("`git commit -m` needs a message".into());
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

/// `HEAD:src/lib.rs` or `main:x` names a file in a revision, not on disk.
fn is_revision_path(arg: &str) -> bool {
    arg.split_once(':')
        .is_some_and(|(rev, _)| !rev.is_empty() && !rev.contains('/'))
}

/// A package, target, test filter or lint name: nothing a shell or a program would read as a
/// path or an option.
fn is_name(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-:,".contains(c))
}

/// A revision or a path, with nothing a shell would expand (`~` or `=` at the start).
fn is_plain(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with(['-', '~', '='])
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:,+@^~".contains(c))
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether `text` names a credential path as whole path components, so `.ssh/id_rsa` matches
/// and `my.sshconfig.txt` does not.
fn touches_secret(text: &str) -> bool {
    let path: String = text
        .chars()
        .map(|c| {
            if c.is_whitespace() || "'\"=:;|&<>()`$".contains(c) {
                '/'
            } else {
                c
            }
        })
        .collect();
    let path = format!("/{path}/");
    SECRET_PATHS.iter().any(|secret| {
        path.contains(&format!("/{secret}/")) || path.contains(&format!("/{secret}."))
    })
}

/// Resolve symlinks in the part of `path` that exists, and `.` and `..` everywhere, so a link
/// can't lead out of the worktree and paths that don't exist yet still work.
fn resolve(path: &Path) -> PathBuf {
    resolve_within(path, 40)
}

fn resolve_within(path: &Path, depth: u32) -> PathBuf {
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
                } else if let Ok(target) = std::fs::read_link(&out) {
                    // A link to something that doesn't exist yet: writing through it creates
                    // its target, so judge the target. Too many links means a loop; treat it as
                    // somewhere no rule allows.
                    if depth == 0 {
                        return PathBuf::from("/");
                    }
                    out.pop();
                    out = resolve_within(&out.join(target), depth - 1);
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
            "git diff main...HEAD",
            "git log --oneline -3 | head -3",
            "git show HEAD:crates/paja/src/lib.rs",
            "pwd",
            "RUST_BACKTRACE=1 cargo test smoke",
            "cargo run -q -- --json work list",
            "cd /h/.paja/envs/work-1 && cargo test",
            "cargo test 2>&1|tail -n 20",
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
            "git commit -aF/proc/self/environ",
            "git commit --fil=/h/.paja/events.jsonl",
            "git add --pathspec-from=/etc/x",
            "cargo fmt -- /h/other/src/main.rs",
            "git rm -r -- -/../..",
            "git log =cargo",
            "git log --format=%H",
            "rm -rf target/debug",
            "ls src",
            "cd ./crates/paja && cargo test",
            "cargo run -q -- --json env create demo",
            "cargo run -q -- run",
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
        assert!(decide("Write", json!({"file_path": "crates/x/.cargo/config.toml"})).is_some());
        assert!(decide("Write", json!({"file_path": "sub/.git/config"})).is_some());
        assert!(decide("Write", json!({"file_path": ".GIT"})).is_some());
        assert!(decide("Write", json!({"file_path": ".Cargo/config.toml"})).is_some());
        assert!(decide("Read", json!({"file_path": "/etc/passwd"})).is_some());
        assert!(decide("Read", json!({"file_path": "/var/run/secrets/token"})).is_some());
        assert!(bash("cargo run -q env create demo").is_some());
        assert!(decide("Glob", json!({"pattern": "{..,x}/../.ssh/*"})).is_some());
        assert!(decide("Glob", json!({"pattern": "**/.ssh/*"})).is_some());
        assert!(decide(
            "Bash",
            json!({"command": "cargo test", "run_in_background": true})
        )
        .is_some());
        assert_eq!(bash("git commit -m 'read my.sshconfig.txt'"), None);
        assert!(decide("Read", json!({"file_path": "~/.paja/events.jsonl"})).is_some());
        assert!(decide("Write", json!({"file_path": "~/.bashrc"})).is_some());
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
        let _ = std::fs::remove_dir_all(&tmp);
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
        // A link to a file that doesn't exist yet would create it outside.
        std::os::unix::fs::symlink(outside.join("new"), root.join("dangling")).unwrap();
        assert!(call("Write", "dangling").is_some());
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
