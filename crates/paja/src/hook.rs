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
//! It is a guardrail, not a sandbox: code the agent writes still runs under `cargo test`
//! (the optional `sbx` sandbox in `sandbox.rs` contains that).
//!
//! It fails closed. [`run`] exits 2 (block) when a rule matches and also when anything goes
//! wrong while deciding, including a panic. Mestari runs a self-test before each agent and
//! refuses to start one unless the hook both blocks a push and allows a build.

use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Result};
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
    /// A reviewer: it reads the change and changes nothing, not even inside the worktree.
    pub read_only: bool,
    /// Dependency sources and the toolchain, readable although they sit outside the worktree:
    /// `$CARGO_HOME/registry`, `$CARGO_HOME/git` and `$RUSTUP_HOME`.
    pub deps: Vec<PathBuf>,
}

impl Bounds {
    /// Both paths must be absolute; Mestari always passes them.
    pub fn new(root: Option<PathBuf>, home: Option<PathBuf>) -> Result<Bounds> {
        match (root, home) {
            (Some(root), Some(home)) if root.is_absolute() && home.is_absolute() => Ok(Bounds {
                root: resolve(&root),
                home: resolve(&home),
                read_only: false,
                deps: dependency_roots(),
            }),
            _ => bail!("`paja hook` needs absolute --root and --home paths; Mestari passes both"),
        }
    }
}

/// Where cargo and rustup keep sources, from the hook's own environment (which the agent
/// cannot change), with their usual defaults under `$HOME`.
fn dependency_roots() -> Vec<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let home = crate::platform::tool_home().filter(|p| p.is_absolute());
    let cargo = var("CARGO_HOME").or_else(|| home.as_ref().map(|h| h.join(".cargo")));
    let rustup = var("RUSTUP_HOME").or_else(|| home.as_ref().map(|h| h.join(".rustup")));
    let mut roots: Vec<PathBuf> = cargo
        .iter()
        .flat_map(|c| [c.join("registry"), c.join("git")])
        .chain(rustup)
        .map(|p| resolve(&p))
        .collect();
    roots.dedup();
    roots
}

/// How long the hook may take before it blocks. Mestari gives Claude Code a longer timeout.
pub const HOOK_DEADLINE_SECS: u64 = 20;

/// `paja hook`: read one tool call on stdin and answer with an exit code Claude Code
/// understands. 0 allows; 2 blocks, with the reason on stderr. Anything but a clear "allow"
/// blocks, a panic included.
pub fn run(
    root: Option<PathBuf>,
    home: Option<PathBuf>,
    stamp: Option<PathBuf>,
    read_only: bool,
) -> ExitCode {
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
    // Proof for Mestari that Claude Code called the hook and it judged the call.
    if let Some(stamp) = &stamp {
        let _ = std::fs::write(stamp, b"");
    }
    let decision = (|| -> Result<Option<String>> {
        // Read the call first, so the caller's write never meets a closed pipe.
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let bounds = Bounds {
            read_only,
            ..Bounds::new(root, home)?
        };
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
    "-F",
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
    // Plain files only: a recursive remove would reach the directories agents never change.
    ("rm", &["-q", "--cached", "-f"]),
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
    // gh's login on Windows: %APPDATA%\GitHub CLI.
    "appdata/roaming/github",
];

/// Tools that touch nothing outside the session. Any tool not named here or handled in
/// [`check`], MCP tools included, is blocked.
/// `StructuredOutput` is how Claude Code returns a `--json-schema` result, such as a verdict.
const HARMLESS_TOOLS: &[&str] = &[
    "TodoWrite",
    "TodoRead",
    "TaskCreate",
    "TaskUpdate",
    "TaskGet",
    "TaskList",
    "ToolSearch",
    "Task",
    "Agent",
    "ExitPlanMode",
    "StructuredOutput",
];

/// Decide one tool call. `Ok(None)` allows it, `Ok(Some(reason))` blocks it.
pub fn check(input: &HookInput, bounds: &Bounds) -> Result<Option<String>> {
    let cwd = resolve(&input.cwd);
    let t = &input.tool_input;
    // File tools expand `~` themselves, so a path starting with it can't be judged here.
    for key in ["file_path", "notebook_path", "path", "pattern", "glob"] {
        if input.tool_name == "Bash" || (input.tool_name == "Grep" && key == "pattern") {
            continue;
        }
        // Patterns hold wildcards, which no name may; they get their own rule below.
        let pattern = matches!(key, "pattern" | "glob");
        if let Some(path) = t[key].as_str() {
            if path.starts_with('~') {
                return Ok(Some(format!(
                    "`{path}` starts with `~`; give an absolute path or one inside the worktree"
                )));
            }
            // Claude Code drops `..` as text, but the kernel follows a symlink before it, so
            // the two can disagree about where such a path lands.
            if Path::new(path)
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return Ok(Some(format!(
                    "`{path}` holds `..`; give the path without it"
                )));
            }
            if touches_secret(path) {
                return Ok(Some(format!(
                    "`{path}` names a credential path; agents never touch those"
                )));
            }
            // Windows spellings the file system reads differently from how they look.
            let problem = if pattern {
                crate::platform::pattern_problem(path)
            } else {
                crate::platform::path_problem(path, false)
            };
            if let Some(why) = problem {
                return Ok(Some(why));
            }
        }
    }
    match input.tool_name.as_str() {
        "Bash" => {
            let Some(command) = t["command"].as_str() else {
                return Ok(Some("a Bash call needs a `command`".into()));
            };
            if t["run_in_background"] == Value::Bool(true) {
                return Ok(Some(
                    "agents run commands in the foreground, so their output comes back".into(),
                ));
            }
            Ok(check_command(command, &cwd, bounds))
        }
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" if bounds.read_only => Ok(Some(
            "reviewers read the change and leave it as it is; say what should change in the \
             verdict"
                .into(),
        )),
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let Some(path) = ["file_path", "notebook_path"]
                .iter()
                .find_map(|key| t[*key].as_str())
            else {
                return Ok(Some("a file edit needs a `file_path`".into()));
            };
            Ok(inside_root("change files", &cwd.join(path), bounds))
        }
        "Read" => {
            let Some(path) = t["file_path"].as_str() else {
                return Ok(Some("a Read needs a `file_path`".into()));
            };
            Ok(check_read(&cwd.join(path), bounds))
        }
        // Searches recurse, so they stay inside the worktree.
        "Glob" | "Grep" | "LS" => {
            let base = t["path"].as_str().map_or(cwd.clone(), |p| cwd.join(p));
            if let Some(reason) = within_root("search", &base, bounds) {
                return Ok(Some(reason));
            }
            // Grep's pattern is a regex; Glob's pattern and Grep's glob are paths.
            let key = if input.tool_name == "Grep" {
                "glob"
            } else {
                "pattern"
            };
            if let Some(pattern) = t[key].as_str() {
                // The literal start of the pattern may be a symlink out of the worktree.
                let start = if pattern.starts_with('/') {
                    PathBuf::from("/")
                } else {
                    PathBuf::new()
                };
                let literal = pattern
                    .split('/')
                    .take_while(|part| !part.contains(['*', '?', '[', '{']))
                    .fold(start, |path, part| path.join(part));
                if let Some(reason) = within_root("search", &base.join(literal), bounds) {
                    return Ok(Some(reason));
                }
                // An absolute pattern is fine when its literal start (checked above) is in the
                // worktree. Brace alternatives may not hold a path of their own.
                let braced_path = pattern
                    .split('{')
                    .skip(1)
                    .any(|alt| alt.split('}').next().is_some_and(|a| a.contains('/')));
                if pattern.contains("..") || braced_path {
                    return Ok(Some(format!(
                        "the pattern `{pattern}` reaches outside the worktree; search relative to it"
                    )));
                }
            }
            // A wildcard can walk into a symlink the literal prefix never names, and an absolute
            // pattern can start anywhere in the worktree, so the whole worktree is checked. A
            // Grep without a `glob` skips what git ignores, as ripgrep does, which keeps a large
            // `target/` from slowing every search.
            let skip = if input.tool_name == "Grep" && t["glob"].is_null() {
                grep_skips(
                    bounds,
                    &base,
                    std::env::var_os("RIPGREP_CONFIG_PATH").is_some(),
                )
            } else {
                Vec::new()
            };
            if let Some(link) = scan(bounds, &skip) {
                return Ok(Some(format!(
                    "{} is a symlink out of the worktree (or nested too deep to check), and \
                     searches here could follow it; search a directory without it, or Read \
                     files by path",
                    crate::platform::shown(&link)
                )));
            }
            Ok(None)
        }
        "WebFetch" | "WebSearch" => Ok(Some("no network access from agents".into())),
        // A subagent's own calls come through this hook, but a worktree of its own is made by
        // Claude Code, in the shared repository, with no call to judge.
        "Task" | "Agent" if !t["isolation"].is_null() || t["run_in_background"] == true => {
            Ok(Some(
                "subagents run in this worktree, in the foreground: leave out `isolation` and \
             `run_in_background`"
                    .into(),
            ))
        }
        name if HARMLESS_TOOLS.contains(&name) => Ok(None),
        name => Ok(Some(format!(
            "agents don't use the `{name}` tool; Mestari's agents use the file tools, cargo and \
             local git"
        ))),
    }
}

/// ripgrep's own ignore files. A `!` line in one can bring back a directory git ignores.
const RG_IGNORE_FILES: &[&str] = &[".ignore", ".rgignore"];

/// The directories a Grep without a `glob` never enters, so the symlink scan may leave them out.
/// Git and ripgrep read ignore files with different engines, so neither is asked: the only
/// directory ever left out is the worktree's own top-level `target`, and only when the
/// worktree's `.gitignore` names it in a line both read the same way (`target`, `target/`,
/// `/target` or `/target/`) and holds no `!` line that could bring it back. Anything else that
/// could make ripgrep search it leaves nothing out:
/// - a ripgrep config file (`rg_config`, from `RIPGREP_CONFIG_PATH`), which can turn ignores off;
/// - a ripgrep ignore file (`.ignore`, `.rgignore`) in the worktree's top directory or above it,
///   or a `.gitignore` above it with a `!` line;
/// - no `.git` in the worktree, without which ripgrep ignores `.gitignore`;
/// - a `.gitignore` that isn't all UTF-8, which ripgrep stops reading at the first bad line, or
///   that holds anything but comments, names and `*`/`?` (no braces, classes, escapes or spaces)
///   or is over 4 KiB, since ripgrep drops a file it can't compile;
/// - a search that starts inside `target`.
fn grep_skips(b: &Bounds, base: &Path, rg_config: bool) -> Vec<PathBuf> {
    let target = b.root.join("target");
    let exists = |path: PathBuf| std::fs::symlink_metadata(path).is_ok();
    let negates = |gitignore: PathBuf| match std::fs::read(&gitignore) {
        Ok(text) => text
            .split(|byte| *byte == b'\n')
            .any(|line| line.starts_with(b"!")),
        Err(err) => err.kind() != std::io::ErrorKind::NotFound,
    };
    let rg_ignore = b.root.ancestors().any(|dir| {
        RG_IGNORE_FILES.iter().any(|name| exists(dir.join(name)))
            || (dir != b.root && negates(dir.join(".gitignore")))
    });
    // ripgrep follows a `.git` link to see whether it exists.
    let git = std::fs::metadata(b.root.join(".git")).is_ok();
    if rg_config || rg_ignore || !git || resolve(base).starts_with(&target) {
        return Vec::new();
    }
    // ripgrep stops reading an ignore file at its first line that isn't UTF-8.
    let Ok(text) = std::fs::read_to_string(b.root.join(".gitignore")) else {
        return Vec::new();
    };
    let lines: Vec<&[u8]> = text
        .as_bytes()
        .split(|byte| *byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .collect();
    // ripgrep drops a whole ignore file whose patterns it can't compile, for example a huge
    // brace alternation, so only a small file of names and `*`/`?` wildcards (and comments) is
    // trusted. ripgrep compiles tens of thousands of wildcards; 4 KiB holds far fewer.
    let plain = |line: &&[u8]| {
        line.is_empty()
            || line.starts_with(b"#")
            || line
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || b"._-/*?".contains(c))
    };
    if text.len() > 4096 || !lines.iter().all(plain) {
        return Vec::new();
    }
    let names = lines
        .iter()
        .any(|line| [&b"target"[..], b"target/", b"/target", b"/target/"].contains(line));
    if names && !lines.iter().any(|line| line.starts_with(b"!")) {
        vec![target]
    } else {
        Vec::new()
    }
}

/// The first symlink out of the worktree, leaving out the directories in `skip`. If the walk
/// finds a symlink into a skipped directory, a search could reach it by that name, so the whole
/// worktree is walked.
fn scan(b: &Bounds, skip: &[PathBuf]) -> Option<PathBuf> {
    let mut unsure = false;
    if let Some(found) = link_out(&b.root, b, skip, &mut unsure, 0) {
        return Some(found);
    }
    if !unsure || skip.is_empty() {
        return None;
    }
    link_out(&b.root, b, &[], &mut unsure, 0)
}

/// The first symlink under `dir` that leads out of the worktree, if any. Git's own directory
/// is skipped: nothing in it links out, and it is large. So are the directories in `skip`;
/// `unsure` is set when something seen means they shouldn't have been.
fn link_out(
    dir: &Path,
    b: &Bounds,
    skip: &[PathBuf],
    unsure: &mut bool,
    depth: usize,
) -> Option<PathBuf> {
    // Deeper than any real source tree: treat it as one, since it can't be checked.
    if depth > 32 {
        return Some(dir.to_path_buf());
    }
    // What can't be listed can't be checked, so it counts as a link out. A worktree that
    // doesn't exist has nothing to search.
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(dir.to_path_buf()),
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return Some(dir.to_path_buf());
        };
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            return Some(path);
        };
        if kind.is_symlink() {
            let target = resolve(&path);
            if !target.starts_with(&b.root) {
                return Some(path);
            }
            if skip.iter().any(|dir| target.starts_with(dir)) {
                *unsure = true;
            }
        } else if kind.is_dir() && entry.file_name() != ".git" && !skip.contains(&path) {
            if let Some(found) = link_out(&path, b, skip, unsure, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

/// `path` must be inside the worktree, and not git's link to the repository.
fn inside_root(what: &str, path: &Path, b: &Bounds) -> Option<String> {
    let full = resolve(path);
    if let Some(reason) = within_root(what, &full, b) {
        return Some(reason);
    }
    // Git and cargo read configuration from these at any depth.
    let inner = full.strip_prefix(&b.root).unwrap_or(&full);
    // Compared without case, for case-insensitive file systems (macOS's default, NTFS), and
    // with Unicode's upper case, as NTFS folds names: `ſ` is `S` there.
    let upper = |text: &str| text.to_uppercase();
    let named = |name: &str| {
        inner
            .components()
            .any(|c| upper(&c.as_os_str().to_string_lossy()) == upper(name))
    };
    if named(".git") {
        return Some(format!(
            "{} is git's own data or link to the repository; agents never change it",
            crate::platform::shown(&full)
        ));
    }
    if named(".github") {
        return Some(format!(
            "{} is CI configuration, which runs with the repository's secrets; agents never \
             change it",
            crate::platform::shown(&full)
        ));
    }
    if named(".claude") {
        return Some(format!(
            "{} is Claude Code configuration for later sessions; agents never change it",
            crate::platform::shown(&full)
        ));
    }
    if named(".cargo") || named("rust-toolchain") || named("rust-toolchain.toml") {
        return Some(format!(
            "{} is cargo or toolchain configuration, which could change how cargo runs; agents \
             never change it",
            crate::platform::shown(&full)
        ));
    }
    if named(".gitattributes") {
        return Some(format!(
            "{} decides how git shows and stores files, so it could hide a change from the \
             reviewer; agents never change it",
            crate::platform::shown(&full)
        ));
    }
    if [
        "rustfmt.toml",
        ".rustfmt.toml",
        "clippy.toml",
        ".clippy.toml",
    ]
    .iter()
    .any(|name| named(name))
    {
        return Some(format!(
            "{} configures the format and lint checks that judge the change; agents never \
             change it",
            crate::platform::shown(&full)
        ));
    }
    None
}

/// `path` must be inside the worktree. Reading git, CI and cargo configuration is fine.
fn within_root(what: &str, path: &Path, b: &Bounds) -> Option<String> {
    let full = resolve(path);
    (!full.starts_with(&b.root)).then(|| {
        format!(
            "agents {what} only inside their own worktree ({}), not {}",
            crate::platform::shown(&b.root),
            crate::platform::shown(&full)
        )
    })
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
            crate::platform::shown(&full)
        ));
    }
    if full.starts_with(&b.home) {
        return Some(format!(
            "{} is the factory's own state; agents never touch it",
            crate::platform::shown(&full)
        ));
    }
    let text = full.to_string_lossy();
    // Inside a dependency root, a crate's own hidden files are fine; anywhere else, hidden
    // directories hold credentials and settings.
    let readable = if let Some(dep) = b.deps.iter().find(|d| full.starts_with(d)) {
        !touches_secret(&full.strip_prefix(dep).unwrap_or(&full).to_string_lossy())
    } else {
        // Cargo's home outside its dependency roots holds its registry token, wherever it is
        // (`/usr/local/cargo` in some images).
        let in_cargo_home = b
            .deps
            .iter()
            .filter(|d| d.file_name().is_some_and(|n| n == "registry" || n == "git"))
            .filter_map(|d| d.parent())
            .any(|home| full.starts_with(home));
        full.starts_with("/usr")
            && !in_cargo_home
            && !touches_secret(&text)
            && !full
                .components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    };
    if !readable {
        return Some(format!(
            "{} is outside the worktree; agents read only the worktree, {}dependency sources \
             ($CARGO_HOME/registry, $CARGO_HOME/git, $RUSTUP_HOME)",
            crate::platform::shown(&full),
            crate::platform::SYSTEM_READS
        ));
    }
    None
}

/// One piece of a command line.
#[derive(Debug, PartialEq)]
enum Token {
    /// A word, and whether any part of it was quoted. Bash never reads a word with quotes
    /// as an assignment: `'RUST_LOG=x' cargo` runs a program named `RUST_LOG=x`.
    Word {
        text: String,
        quoted: bool,
    },
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
    let mut quoted = false;
    let mut chars = command.chars().peekable();
    let flush =
        |word: &mut String, in_word: &mut bool, quoted: &mut bool, tokens: &mut Vec<Token>| {
            if *in_word {
                tokens.push(Token::Word {
                    text: std::mem::take(word),
                    quoted: std::mem::take(quoted),
                });
                *in_word = false;
            }
        };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                quoted = true;
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
                quoted = true;
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
            ' ' | '\t' => flush(&mut word, &mut in_word, &mut quoted, &mut tokens),
            '&' if chars.peek() == Some(&'&') => {
                chars.next();
                flush(&mut word, &mut in_word, &mut quoted, &mut tokens);
                tokens.push(Token::And);
            }
            '|' if chars.peek() != Some(&'|') => {
                flush(&mut word, &mut in_word, &mut quoted, &mut tokens);
                tokens.push(Token::Pipe);
            }
            '2' if !in_word && next_is(&mut chars, ">&1") => {
                tokens.push(Token::StderrToStdout);
            }
            // `~` only inside a word (`HEAD~1`); at its start the shell would expand it.
            c if c.is_ascii_alphanumeric()
                || "-_./=:,+@%^".contains(c)
                || (c == '~' && in_word) =>
            {
                in_word = true;
                word.push(c);
            }
            '\n' => return Err("a newline".into()),
            c => return Err(format!("`{c}` outside quotes")),
        }
    }
    flush(&mut word, &mut in_word, &mut quoted, &mut tokens);
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
    let tokens = match tokenize(command) {
        Ok(tokens) => tokens,
        Err(what) => return Some(format!("the command has {what}; {SHAPE}")),
    };
    // Every word but a commit message may not name a credential path.
    let mut message = false;
    for token in &tokens {
        let Token::Word { text: word, .. } = token else {
            continue;
        };
        if std::mem::take(&mut message) {
            continue;
        }
        message = matches!(word.as_str(), "-m" | "--message");
        if touches_secret(word) {
            return Some("that command names credentials; agents never touch them".into());
        }
    }
    // `&&` steps, each a pipeline of simple commands.
    let mut here = cwd.to_path_buf();
    for (i, step) in tokens.split(|t| *t == Token::And).enumerate() {
        let stages: Vec<&[Token]> = step.split(|t| *t == Token::Pipe).collect();
        // A `cd` only moves the shell when it is the whole first step.
        let moves = i == 0 && stages.len() == 1;
        for (j, stage) in stages.into_iter().enumerate() {
            let mut words: Vec<&str> = Vec::new();
            // Leading unquoted `NAME=value` words, which bash reads as assignments.
            let mut assignments = 0;
            for (k, token) in stage.iter().enumerate() {
                match token {
                    Token::Word { text, quoted } => {
                        if words.len() == assignments && !quoted && is_assignment(text) {
                            assignments += 1;
                        }
                        words.push(text);
                    }
                    Token::StderrToStdout if k == stage.len() - 1 => {}
                    _ => return Some(format!("a misplaced operator; {SHAPE}")),
                }
            }
            let reason = if j == 0 {
                check_simple(&words, assignments, moves, &mut here, b)
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

/// `words[..start]` are the assignments in front of the program.
fn check_simple(
    words: &[&str],
    start: usize,
    first: bool,
    here: &mut PathBuf,
    b: &Bounds,
) -> Option<String> {
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
    // Cargo and git act on the directory they run in, which must be the worktree.
    if matches!(*program, "cargo" | "git") && !here.starts_with(&b.root) {
        return Some(format!(
            "the shell is in {}, outside the worktree; start with `cd '{}' &&`",
            crate::platform::shown(here),
            crate::platform::shown(&b.root)
        ));
    }
    match *program {
        "cargo" => check_cargo(args, b.read_only),
        "git" => check_git(args, here, b),
        "pwd" if args.is_empty() => None,
        // Only into the worktree's root, and only as a step of its own, so a failed `cd`
        // stops the rest. No `.` or `..` parts: bash's `cd` drops `..` as text before it
        // follows links, so `link/..` would land beside the link, not where `resolve` says.
        "cd" if first => match args {
            [target]
                if !target
                    .split(['/', '\\'])
                    .any(|part| part == "." || part == "..")
                    && crate::platform::path_problem(
                        &crate::platform::shell_path(target).to_string_lossy(),
                        false,
                    )
                    .is_none()
                    && crate::platform::shell_path(target).is_absolute()
                    && resolve(&crate::platform::shell_path(target)) == b.root =>
            {
                *here = b.root.clone();
                None
            }
            _ => Some(format!(
                "`cd` only goes to the worktree's root: `cd '{}'`",
                crate::platform::shown(&b.root)
            )),
        },
        "cd" => Some("`cd` may only be the first step of a command, on its own".into()),
        other => Some(format!("`{other}` is not allowed; {SHAPE}")),
    }
}

fn check_cargo(args: &[&str], read_only: bool) -> Option<String> {
    let sub = args.first().copied().unwrap_or("");
    if !CARGO_SUBCOMMANDS.contains(&sub) {
        return Some(format!(
            "`cargo {sub}` is not allowed; agents use cargo {}",
            CARGO_SUBCOMMANDS.join(", ")
        ));
    }
    // `cargo fmt` rewrites sources unless it only checks them; a reviewer changes nothing.
    if read_only && sub == "fmt" && !args.contains(&"--check") {
        return Some("reviewers change nothing: use `cargo fmt --all --check`".into());
    }
    // `--all` and `-p` can also format path dependencies, which may sit outside the
    // worktree, so formatting (as opposed to checking) covers the workspace only.
    if sub == "fmt"
        && !args.contains(&"--check")
        && !args[1..]
            .iter()
            .all(|a| matches!(*a, "-q" | "--quiet" | "-v" | "--verbose"))
    {
        return Some(
            "`cargo fmt` with options could rewrite path dependencies outside the worktree; \
             use plain `cargo fmt` to format, or `cargo fmt --all --check` to check"
                .into(),
        );
    }
    let rest = &args[1..];
    let split = rest.iter().position(|a| *a == "--").unwrap_or(rest.len());
    let (own, after) = (&rest[..split], rest.get(split + 1..).unwrap_or(&[]));
    // A feature may belong to a dependency (`serde/derive`); every other value is a plain name.
    let value_ok = |flag: &str, value: &str| {
        is_name(value)
            || matches!(flag, "-F" | "--features")
                && value
                    .split_once('/')
                    .is_some_and(|(krate, feature)| is_name(krate) && is_name(feature))
    };
    let mut value_next = None;
    for arg in own {
        let ok = if let Some(flag) = value_next.take() {
            value_ok(flag, arg)
        } else if CARGO_FLAGS.contains(arg) {
            true
        } else if CARGO_NAME_FLAGS.contains(arg) {
            value_next = Some(*arg);
            true
        } else if let Some((flag, value)) = arg.split_once('=') {
            CARGO_NAME_FLAGS.contains(&flag) && value_ok(flag, value)
        } else {
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
    if let Some(flag) = value_next {
        return Some(format!("`cargo {sub} {flag}` is missing its value"));
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
    if lint_next {
        return Some(format!(
            "`cargo {sub} --`: a lint flag is missing its lint name"
        ));
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
    if b.read_only && !matches!(sub, "diff" | "show" | "log" | "status") {
        return Some(format!(
            "`git {sub}` changes the branch; reviewers only read it (diff, show, log, status)"
        ));
    }
    let mut paths_only = false;
    let mut skip_next = false;
    let mut message = false;
    for arg in &args[1..] {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if !paths_only && sub == "commit" && matches!(*arg, "-m" | "--message") {
            // A commit message is free text, neither a flag nor a path.
            skip_next = true;
            message = true;
        } else if !paths_only && *arg == "--" {
            paths_only = true;
        } else if !paths_only && arg.starts_with('-') {
            let count = arg.len() > 1 && arg[1..].chars().all(|c| c.is_ascii_digit());
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
        } else {
            // Every word stays inside the root when read as a path: a revision such as
            // `main...HEAD` or `HEAD:src/lib.rs` does, while one that leaves it would let git
            // read a file outside the worktree (two outside paths make `git diff` compare
            // files directly).
            let reads = matches!(sub, "diff" | "show" | "log" | "status");
            // Reads take revisions (`HEAD:path`, `main..`); add, rm and commit take only paths.
            if let Some(why) = crate::platform::path_problem(arg, reads) {
                return Some(why);
            }
            let target = here.join(arg);
            // Only reads take revisions; add, rm and commit paths get the full write check.
            let reason = if reads {
                within_root("use git", &target, b)
            } else {
                inside_root("use git", &target, b)
            };
            if let Some(reason) = reason {
                return Some(reason);
            }
        }
    }
    if skip_next || (sub == "commit" && !message) {
        // Without one, git would open an editor and the headless agent would hang.
        return Some("`git commit` needs `-m '<message>'`".into());
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
    // Without case, and with `\\` as a separator, for Windows and macOS file systems.
    let path: String = text
        .chars()
        .map(|c| {
            if c.is_whitespace() || "'\"=:;|&<>()`$\\".contains(c) {
                "/".to_string()
            } else {
                c.to_uppercase().collect()
            }
        })
        .collect();
    let path = format!("/{path}/");
    SECRET_PATHS.iter().any(|secret| {
        let secret = secret.to_uppercase();
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
            // A drive on its own (`C:`) is that drive's current directory; don't resolve it.
            prefix @ Component::Prefix(_) => out.push(prefix),
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

/// The calls Mestari's self-test sends, run in `root`: one that must be blocked and a build
/// that must be allowed. A builder's hook must block a push; a reviewer's must block even a
/// local commit, which proves it runs read-only.
pub fn self_test_inputs(root: &Path, read_only: bool) -> (String, String) {
    let call = |command: &str| {
        serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": command},
            "cwd": root
        })
        .to_string()
    };
    let blocked = if read_only {
        "git commit -q -m self-test"
    } else {
        "git push origin main"
    };
    (call(blocked), call("cargo test"))
}

// The cases use Unix paths; `windows_tests` below covers the Windows spellings.
#[cfg(test)]
mod scan_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_large_ignored_target_keeps_searches_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("home").join("envs").join("a");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success();
        assert!(ok, "git init");
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        // About as many entries as a few builds leave behind.
        for d in 0..100 {
            let dir = root.join("target").join("debug").join(format!("d{d}"));
            std::fs::create_dir_all(&dir).unwrap();
            for f in 0..1000 {
                std::fs::write(dir.join(format!("f{f}")), "").unwrap();
            }
        }
        let b = Bounds::new(Some(root.clone()), Some(tmp.path().join("home"))).unwrap();
        let input = HookInput {
            tool_name: "Grep".into(),
            tool_input: json!({"pattern": "x"}),
            cwd: root.clone(),
        };
        let start = std::time::Instant::now();
        assert_eq!(check(&input, &b).unwrap(), None);
        let took = start.elapsed();
        assert!(took < std::time::Duration::from_secs(1), "took {took:?}");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOT: &str = "/h/.paja/envs/work-1";

    fn bounds() -> Bounds {
        Bounds {
            deps: ["/h/.cargo/registry", "/h/.cargo/git", "/h/.rustup"]
                .map(PathBuf::from)
                .to_vec(),
            ..Bounds::new(Some(ROOT.into()), Some("/h/.paja".into())).unwrap()
        }
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
            "git diff HEAD~1 -- .github/workflows/ci.yml",
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
    fn a_quoted_word_is_never_an_assignment() {
        assert_eq!(bash("RUST_LOG=debug cargo test"), None);
        for bad in [
            "'RUST_LOG=x' cargo test",
            "\"RUST_LOG=x\" cargo test",
            "RUST_LOG'=x' cargo test",
        ] {
            assert!(bash(bad).is_some(), "{bad}");
        }
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
            "git rm -r -f .",
            "git commit",
            "git commit -a -- -m",
            "git diff a:/x a:/../../../../../../h/.paja/events.jsonl",
            "git rm -f -q a:/../.github/workflows/ci.yml",
            "git add a:/../.cargo/config.toml",
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
        assert!(decide("Write", json!({"file_path": ".claude/settings.json"})).is_some());
        assert_eq!(bash("git commit -m 'ignore .docker/ in packaging'"), None);
        assert!(decide("Write", json!({"file_path": ".github/workflows/x.yml"})).is_some());
        assert!(bash("git rm -r -f x:/../.cargo").is_some());
        assert!(bash("git log -").is_some());
        assert!(decide("Write", json!({"file_path": ".Cargo/config.toml"})).is_some());
        assert!(decide("Read", json!({"file_path": "/etc/passwd"})).is_some());
        assert_eq!(
            decide(
                "Read",
                json!({"file_path": "/h/.cargo/registry/src/i/serde-1.0/.cargo_vcs_info.json"})
            ),
            None
        );
        assert!(decide("Read", json!({"file_path": "/var/run/secrets/token"})).is_some());
        assert!(bash("cargo run -q env create demo").is_some());
        assert!(decide("Glob", json!({"pattern": "{..,x}/../.ssh/*"})).is_some());
        assert!(decide("Glob", json!({"pattern": "{a/b,c}/*"})).is_some());
        assert!(decide("Glob", json!({"pattern": "/etc/*"})).is_some());
        assert!(decide("Glob", json!({"pattern": "/h/.paja/envs/work-2/**"})).is_some());
        assert!(bash("cargo clippy -- -D").is_some());
        assert_eq!(decide("Glob", json!({"pattern": "*.{rs,toml}"})), None);
        assert_eq!(
            decide("Glob", json!({"pattern": format!("{ROOT}/**/*.rs")})),
            None
        );
        assert!(decide("Glob", json!({"pattern": "**/.ssh/*"})).is_some());
        assert!(decide("Write", json!({"file_path": "rust-toolchain.toml"})).is_some());
        assert!(decide("Write", json!({"file_path": "rustfmt.toml"})).is_some());
        assert!(decide("Write", json!({"file_path": "src/.gitattributes"})).is_some());
        let usr_cargo = Bounds {
            deps: vec![
                "/usr/local/cargo/registry".into(),
                "/usr/local/cargo/git".into(),
            ],
            ..bounds()
        };
        let read = |path: &str| {
            let input = HookInput {
                tool_name: "Read".into(),
                tool_input: json!({ "file_path": path }),
                cwd: ROOT.into(),
            };
            check(&input, &usr_cargo).unwrap()
        };
        assert!(read("/usr/local/cargo/credentials.toml").is_some());
        assert_eq!(read("/usr/local/cargo/registry/src/x/lib.rs"), None);
        assert_eq!(read("/usr/include/stdio.h"), None);
        assert!(decide("Write", json!({"file_path": "crates/x/.clippy.toml"})).is_some());
        assert!(decide("Write", json!({"file_path": format!("{ROOT}/src/../x.rs")})).is_some());
        assert_eq!(decide("TaskCreate", json!({"subject": "x"})), None);
        let elsewhere = HookInput {
            tool_name: "Bash".into(),
            tool_input: json!({"command": "git add -A"}),
            cwd: "/h/other".into(),
        };
        assert!(check(&elsewhere, &bounds()).unwrap().is_some());
        let back = HookInput {
            tool_name: "Bash".into(),
            tool_input: json!({"command": format!("cd {ROOT} && git add -A")}),
            cwd: "/h/other".into(),
        };
        assert_eq!(check(&back, &bounds()).unwrap(), None);
        assert!(decide("Edit", json!({"file_path": "crates/x/Rust-Toolchain"})).is_some());
        assert_eq!(bash("cargo test --features serde/derive"), None);
        assert_eq!(bash("cargo test --features=serde/derive"), None);
        assert!(bash("cargo test --features ../x").is_some());
        assert!(bash("cargo test -p serde/derive").is_some());
        assert_eq!(bash("cargo test -F serde/derive"), None);
        assert!(bash("cargo fmt --all").is_some());
        assert_eq!(bash("cargo fmt"), None);
        assert_eq!(bash("cargo fmt --all --check"), None);
        assert!(bash("cargo fmt -p other").is_some());
        // `-e` would take the pattern and leave the last word to be read as a file.
        assert!(bash("cargo test 2>&1 | grep -ve -qqq src/lib.rs").is_some());
        assert!(bash("cargo test 2>&1 | grep -e x src/lib.rs").is_some());
        assert!(decide("Read", json!({"file_path": "/opt/.cargo/registry/x"})).is_some());
        assert!(decide(
            "Bash",
            json!({"command": "cargo test", "run_in_background": true})
        )
        .is_some());
        assert_eq!(bash("git commit -m 'read my.sshconfig.txt'"), None);
        assert_eq!(
            bash("git add -A && git commit -q -m 'x' -m 'Document .ssh/config support'"),
            None
        );
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
        assert_eq!(decide("Agent", json!({"prompt": "look"})), None);
        assert!(decide("Agent", json!({"prompt": "x", "isolation": "worktree"})).is_some());
        assert!(decide("Task", json!({"prompt": "x", "run_in_background": true})).is_some());
        assert_eq!(decide("StructuredOutput", json!({"verdict": "land"})), None);
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
        let glob = HookInput {
            tool_name: "Glob".into(),
            tool_input: json!({"pattern": "link/**/*.rs"}),
            cwd: root.clone(),
        };
        assert!(check(&glob, &b).unwrap().is_some());
        // Wildcards and braces can name the link without spelling it out.
        for pattern in ["l[i]nk/**", "{link,x}/*", "**/*.rs"] {
            let glob = HookInput {
                tool_name: "Glob".into(),
                tool_input: json!({ "pattern": pattern }),
                cwd: root.clone(),
            };
            assert!(check(&glob, &b).unwrap().is_some(), "{pattern}");
        }
        let grep = HookInput {
            tool_name: "Grep".into(),
            tool_input: json!({"pattern": "x", "glob": "*"}),
            cwd: root.clone(),
        };
        assert!(check(&grep, &b).unwrap().is_some());
        assert_eq!(call("Write", "src/new/x.rs"), None);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn a_grep_skips_only_a_target_ripgrep_skips_too() {
        use std::os::unix::fs::symlink;
        let tmp = std::env::temp_dir().join(format!("paja-hook-ignored-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("home/envs/a");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        // A link out inside the ignored directory, where a Grep without a glob never goes.
        symlink(&outside, root.join("target/debug/out")).unwrap();
        let b = Bounds::new(Some(root.clone()), Some(tmp.join("home"))).unwrap();
        let search = |tool: &str, input: Value| {
            let input = HookInput {
                tool_name: tool.into(),
                tool_input: input,
                cwd: root.clone(),
            };
            check(&input, &b).unwrap()
        };
        let grep = || search("Grep", json!({"pattern": "x"}));
        assert_eq!(grep(), None);
        // Globs override ignores in ripgrep, and Glob and LS don't use them at all.
        assert!(search("Grep", json!({"pattern": "x", "glob": "*.rs"})).is_some());
        assert!(search("Glob", json!({"pattern": "*/debug/**"})).is_some());
        assert!(search("LS", json!({"path": "."})).is_some());
        // A search that starts inside the ignored directory searches it.
        assert!(search("Grep", json!({"pattern": "x", "path": "target"})).is_some());
        assert!(search("Grep", json!({"pattern": "x", "path": "target/debug"})).is_some());
        // Each spelling both engines read the same way, line ends included.
        for line in ["/target\n", "target", "/target/\r\n"] {
            std::fs::write(root.join(".gitignore"), line).unwrap();
            assert_eq!(grep(), None, "{line:?}");
        }
        let gitignore = |text: &str| std::fs::write(root.join(".gitignore"), text).unwrap();
        // ripgrep's own ignore files in the worktree's top directory, or above it, can bring
        // `target` back, as can any `!` line.
        for name in [".ignore", ".rgignore"] {
            for dir in [root.clone(), tmp.clone()] {
                std::fs::write(dir.join(name), "!target/\n").unwrap();
                assert!(grep().is_some(), "{name} in {}", dir.display());
                std::fs::remove_file(dir.join(name)).unwrap();
            }
        }
        // ripgrep drops a file it can't compile, and the hook doesn't compile it, so only
        // names and simple wildcards count.
        let braces: Vec<String> = (0..2000).map(|n| format!("x{n}")).collect();
        for text in [
            format!("target/\n{{{}}}\n", braces.join(",")),
            "target/\n[ab].log\n".to_string(),
            format!("target/\n#{}\n", "x".repeat(5000)),
        ] {
            gitignore(&text);
            assert!(grep().is_some(), "{}", &text[..20]);
        }
        gitignore("# build output\n/target\n__pycache__/\n*.pyc\n**/out?.log\n");
        assert_eq!(grep(), None, "comments, names and simple wildcards");
        std::fs::write(root.join(".gitignore"), b"\xff\ntarget/\n").unwrap();
        assert!(
            grep().is_some(),
            "a line ripgrep can't read before `target/`"
        );
        gitignore("target/\n!keep\n");
        assert!(grep().is_some(), "a `!` line");
        std::fs::write(tmp.join(".gitignore"), "!target\n").unwrap();
        assert!(grep().is_some(), "a `!` line above the worktree");
        std::fs::remove_file(tmp.join(".gitignore")).unwrap();
        gitignore("target/\n");
        // Without `.git`, ripgrep doesn't read `.gitignore` at all.
        std::fs::rename(root.join(".git"), root.join("git-moved")).unwrap();
        assert!(grep().is_some(), "no .git");
        symlink(root.join("nowhere"), root.join(".git")).unwrap();
        assert!(grep().is_some(), "a dangling .git link");
        std::fs::remove_file(root.join(".git")).unwrap();
        std::fs::rename(root.join("git-moved"), root.join(".git")).unwrap();
        assert_eq!(grep(), None);
        // Nothing else is left out, however git reads it: a directory whose contents alone are
        // ignored (ripgrep enters it and reads its ignore files), a pattern ripgrep's globs
        // don't parse the same way, a `target` below a fake nested repository (ripgrep stops
        // applying the top `.gitignore` there), and what only git's `info/exclude` ignores.
        gitignore("target\na/*\nt[[:alpha:]]x/\nb/\n");
        std::fs::write(root.join(".git/info/exclude"), "c/\n").unwrap();
        for dir in ["a", "tbx", "n/target", "c"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            symlink(&outside, root.join(dir).join("out")).unwrap();
            assert!(grep().is_some(), "{dir}");
            std::fs::remove_dir_all(root.join(dir)).unwrap();
        }
        std::fs::create_dir_all(root.join("n")).unwrap();
        std::fs::write(root.join("n/.git"), "").unwrap();
        std::fs::create_dir_all(root.join("n/target")).unwrap();
        symlink(&outside, root.join("n/target/out")).unwrap();
        assert!(grep().is_some(), "a fake nested repository");
        std::fs::remove_dir_all(root.join("n")).unwrap();
        gitignore("target/\n");
        assert_eq!(grep(), None);
        assert!(grep_skips(&b, &root, true).is_empty());
        assert!(!grep_skips(&b, &root, false).is_empty());
        // A link inside the worktree into the ignored directory reaches it by another name.
        symlink(root.join("target"), root.join("src/via")).unwrap();
        assert!(grep().is_some(), "a link into the ignored directory");
        std::fs::remove_file(root.join("src/via")).unwrap();
        // ripgrep matches with case, so `TARGET` is searched.
        git(&["config", "core.ignoreCase", "true"]);
        std::fs::create_dir_all(root.join("TARGET")).unwrap();
        symlink(&outside, root.join("TARGET/out")).unwrap();
        assert!(grep().is_some(), "TARGET is not ignored by ripgrep");
        std::fs::remove_dir_all(root.join("TARGET")).unwrap();
        assert_eq!(grep(), None);
        // The ignored name itself may be the link out.
        std::fs::remove_dir_all(root.join("target")).unwrap();
        symlink(&outside, root.join("target")).unwrap();
        assert!(grep().is_some(), "target as a link out");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn cd_takes_the_root_only_without_dot_parts() {
        let tmp = std::env::temp_dir().join(format!("paja-hook-cd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("home/envs/a");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        // A link outside the worktree to a directory inside it: `resolve` reads `L/..` as the
        // root, but bash's `cd` drops `..` first and lands in the link's own parent.
        std::os::unix::fs::symlink(root.join("sub"), tmp.join("L")).unwrap();
        let b = Bounds::new(Some(root.clone()), Some(tmp.join("home"))).unwrap();
        let bash = |command: String| {
            let input = HookInput {
                tool_name: "Bash".into(),
                tool_input: json!({ "command": command }),
                cwd: root.clone(),
            };
            check(&input, &b).unwrap()
        };
        let root = b.root.display().to_string();
        let link = tmp.join("L").display().to_string();
        for blocked in [
            format!("cd {root}/sub/.. && git status"),
            format!("cd {link}/.."),
            format!("cd {link}/.. && git status"),
            format!("cd {root}/. && git status"),
            format!("cd {root}/./ && git status"),
        ] {
            assert!(bash(blocked.clone()).is_some(), "{blocked} was allowed");
        }
        assert_eq!(bash(format!("cd {root} && git status")), None);
        assert_eq!(bash(format!("cd {root}/ && git status")), None);
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
    fn reviewers_only_read() {
        let b = Bounds {
            read_only: true,
            ..bounds()
        };
        let call = |tool: &str, input: Value| {
            let input = HookInput {
                tool_name: tool.into(),
                tool_input: input,
                cwd: ROOT.into(),
            };
            check(&input, &b).unwrap()
        };
        assert!(call("Edit", json!({"file_path": "src/lib.rs"})).is_some());
        assert!(call("Bash", json!({"command": "git add -A"})).is_some());
        assert!(call("Bash", json!({"command": "git commit -m x"})).is_some());
        assert_eq!(
            call("Bash", json!({"command": "git diff main...HEAD"})),
            None
        );
        assert_eq!(call("Read", json!({"file_path": "src/lib.rs"})), None);
        assert!(call("Bash", json!({"command": "cargo fmt --all"})).is_some());
        assert_eq!(
            call("Bash", json!({"command": "cargo fmt --all --check"})),
            None
        );
        assert_eq!(call("Bash", json!({"command": "cargo test"})), None);
    }

    #[test]
    fn malformed_calls_are_errors() {
        let input = HookInput {
            tool_name: "Bash".into(),
            tool_input: json!({}),
            cwd: ROOT.into(),
        };
        assert!(check(&input, &bounds()).unwrap().is_some());
        assert!(Bounds::new(Some("rel".into()), Some("/h".into())).is_err());
        assert!(Bounds::new(None, Some("/h".into())).is_err());
    }

    #[test]
    fn the_self_test_blocks_a_push_and_allows_a_build() {
        for read_only in [false, true] {
            let b = Bounds {
                read_only,
                ..bounds()
            };
            let (blocked, build) = self_test_inputs(Path::new(ROOT), read_only);
            let blocked: HookInput = serde_json::from_str(&blocked).unwrap();
            let build: HookInput = serde_json::from_str(&build).unwrap();
            assert!(check(&blocked, &b).unwrap().is_some());
            assert!(check(&build, &b).unwrap().is_none());
        }
        // The reviewer's blocked call is one a builder may make.
        let (commit, _) = self_test_inputs(Path::new(ROOT), true);
        let commit: HookInput = serde_json::from_str(&commit).unwrap();
        assert!(check(&commit, &bounds()).unwrap().is_none());
    }
}

// The same rules on Windows spellings: drive letters, `\` separators, case-insensitive names
// and the forms the file system reads differently from how they look.
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use serde_json::json;

    struct Env {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        bounds: Bounds,
    }

    /// A real worktree directory, so the hook's path resolution sees what Windows sees.
    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("paja");
        let root = home.join("envs").join("work-1");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src").join("lib.rs"), "").unwrap();
        let bounds = Bounds::new(Some(root.clone()), Some(home.clone())).unwrap();
        Env {
            _tmp: tmp,
            root,
            home,
            bounds,
        }
    }

    impl Env {
        fn decide(&self, tool: &str, input: Value) -> Option<String> {
            let input = HookInput {
                tool_name: tool.into(),
                tool_input: input,
                cwd: self.root.clone(),
            };
            check(&input, &self.bounds).unwrap()
        }
        fn write(&self, path: &str) -> Option<String> {
            self.decide("Write", json!({"file_path": path, "content": ""}))
        }
        fn at(&self, rest: &str) -> String {
            format!("{}\\{rest}", self.root.display())
        }
        fn bash(&self, command: &str) -> Option<String> {
            self.decide("Bash", json!({ "command": command }))
        }
    }

    #[test]
    fn a_quoted_word_is_never_an_assignment() {
        let e = env();
        assert_eq!(e.bash("RUST_LOG=debug cargo test"), None);
        for bad in ["'RUST_LOG=x' cargo test", "\"RUST_LOG=x\" cargo test"] {
            assert!(e.bash(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn plain_windows_paths_inside_the_worktree_are_allowed() {
        let e = env();
        assert_eq!(e.write(&e.at("src\\new.rs")), None);
        assert_eq!(e.write(&e.at("src/new.rs")), None);
        assert_eq!(e.write("src\\new.rs"), None);
        assert_eq!(
            e.decide("Read", json!({"file_path": e.at("src\\lib.rs")})),
            None
        );
    }

    #[test]
    fn protected_names_are_matched_without_case_or_tricks() {
        let e = env();
        for path in [
            ".GIT\\config",
            ".Github\\workflows\\ci.yml",
            ".gitattributes.",
            ".gitattributes ",
            ".gitattributes::$DATA",
            "src\\lib.rs:hidden",
            "NUL",
            "src\\con.txt",
            "a?b",
            "COM¹",
            ".gitattributeſ",
        ] {
            assert!(e.write(&e.at(path)).is_some(), "{path} was allowed");
        }
    }

    #[test]
    fn device_unc_and_driveless_paths_are_refused() {
        let e = env();
        let verbatim = format!("\\\\?\\{}", e.at("src\\lib.rs"));
        for path in [
            verbatim.as_str(),
            "\\\\server\\share\\x",
            "//server/share/x",
            "C:foo",
            "\\Windows\\x",
            "/c/Windows/x",
        ] {
            assert!(e.write(path).is_some(), "{path} was allowed");
        }
    }

    #[test]
    fn reads_outside_the_worktree_and_credentials_are_refused() {
        let e = env();
        for path in [
            "C:\\Windows\\win.ini",
            "C:\\Users\\me\\.ssh\\id_rsa",
            "C:\\Users\\me\\.SSH\\id_rsa",
            "C:\\Users\\me\\AppData\\Roaming\\GitHub CLI\\hosts.yml",
        ] {
            assert!(
                e.decide("Read", json!({ "file_path": path })).is_some(),
                "{path} was allowed"
            );
        }
        let state = e.home.join("events.jsonl");
        let why = e
            .decide("Read", json!({"file_path": state.display().to_string()}))
            .unwrap();
        assert!(why.contains("factory's own state"), "{why}");
    }

    #[test]
    fn cd_takes_the_root_in_windows_or_git_bash_spelling() {
        let e = env();
        let windows = e.root.display().to_string();
        assert_eq!(e.bash(&format!("cd '{windows}' && cargo test")), None);
        let drive = windows.chars().next().unwrap().to_ascii_lowercase();
        let msys = format!("/{drive}{}", windows[2..].replace('\\', "/"));
        assert_eq!(e.bash(&format!("cd {msys} && cargo test")), None);
        assert!(e.bash("cd C:\\Windows && cargo test").is_some());
        // `.` and `..` parts are refused in either spelling, even when they lead to the root.
        assert!(e
            .bash(&format!("cd '{windows}\\src\\..' && cargo test"))
            .is_some());
        assert!(e.bash(&format!("cd {msys}/src/.. && cargo test")).is_some());
        assert!(e
            .bash(&format!("cd '{windows}\\.' && cargo test"))
            .is_some());
        assert_eq!(e.bash(&format!("cd '{windows}\\' && cargo test")), None);
        assert!(e.bash("cd /c/Windows && cargo test").is_some());
        // The advice can be followed as given: a plain, quoted path, no `\\?\`.
        let why = e.bash("cd /c/Windows && cargo test").unwrap();
        let advice = why.split('`').nth(3).unwrap();
        assert!(!advice.contains(r"\\?\"), "{why}");
        assert_eq!(e.bash(&format!("{advice} && cargo test")), None, "{why}");
    }

    #[test]
    fn git_paths_stay_inside_and_revisions_keep_their_colons() {
        let e = env();
        assert_eq!(e.bash("git show HEAD:src/lib.rs"), None);
        assert_eq!(e.bash("git log main.."), None);
        assert!(e.bash("git add .gitattributes.").is_some());
        assert!(e.bash("git add .gitattributes..").is_some());
        assert!(e.bash("git diff C:/Windows/win.ini").is_some());
        assert!(e.bash("git diff /c/Windows/win.ini").is_some());
    }

    #[test]
    fn searches_with_backslashes_or_drives_are_refused() {
        let e = env();
        assert_eq!(e.decide("Glob", json!({"pattern": "src/**/*.rs"})), None);
        assert!(e
            .decide("Glob", json!({"pattern": "C:\\Users\\*"}))
            .is_some());
        assert!(e.decide("Glob", json!({"pattern": "..\\*"})).is_some());
        assert!(e
            .decide("Grep", json!({"pattern": "x", "glob": "C:/Users/**"}))
            .is_some());
    }

    #[test]
    fn a_junction_out_of_the_worktree_blocks_searches() {
        let e = env();
        let outside = e.home.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(e.root.join("out"))
            .arg(&outside)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert!(e.decide("Glob", json!({"pattern": "**/*.rs"})).is_some());
        assert!(e.write(&e.at("out\\x.rs")).is_some());
    }
}
