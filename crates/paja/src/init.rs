//! `paja init`: set an existing repository up for Paja in one pull request.
//!
//! An agent on the `init` role reads the default branch and proposes the stacks, checks, commands,
//! CI setup, high-risk paths, toolchains and services, whatever the languages; Paja keeps only what
//! the files on the default branch back up (see `detect`). The built-in table of known files (Rust,
//! Node, Python, Go) is the fallback when `--no-agent` is given or the agent can't run. From that
//! it proposes `.paja/checks`, `.paja/commands`, `.paja/models` and `.paja/high-risk`, adds a
//! `CLAUDE.md` (or a "Working with Paja" section to the repository's own), maybe a CI workflow,
//! creates the work label, and opens a pull request from the branch `paja/init`. The files are
//! written in a temporary worktree of the default branch, so the person's checkout is never
//! touched, and nothing is pushed to the default branch.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::ahjo::{self, git_command};
use crate::home::Home;
use crate::jono::{self, Labels, LABELS_FILE};

mod detect;
mod scaffold;
pub use detect::{Check, Dropped, Environment, Service, Source, StackDir, Toolchain};
pub use scaffold::{run_new, NewOptions, NewReport, STACKS};

/// The branch the pull request comes from.
pub const BRANCH: &str = "paja/init";

const CHECKS_FILE: &str = ".paja/checks";
const COMMANDS_FILE: &str = ".paja/commands";
const MODELS_FILE: &str = ".paja/models";
const HIGH_RISK_FILE: &str = ".paja/high-risk";
const CI_FILE: &str = ".paja/ci";
const WORKFLOW_FILE: &str = ".github/workflows/paja.yml";
const CLAUDE_FILE: &str = "CLAUDE.md";
const SECTION: &str = "## Working with Paja";

const MODELS: &str = "\
# The model, and optionally the effort, each agent role runs on: `role model [effort]`.
# Mestari reads this file from main, never from the branch under review.
# Roles: build (first build round), fix (later build rounds), review (first full review),
# re-review (normal-risk fix-only review), review-2 (second high-risk reviewer), triage
# (`paja triage`), plan (the pool's planner) and init (`paja init`'s detection of the project).
# A `fallback <model>` line names a model to use when the chosen one is busy.
# A role with no line runs on the account's default model.
build sonnet
fix sonnet
review opus
re-review sonnet medium
review-2 sonnet medium
# The most turns one session of a role may take: `max-turns <role> <turns>`.
max-turns build 80
max-turns fix 80
max-turns review 30
max-turns re-review 30
max-turns review-2 30
";

/// What `paja init` can change.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Write a GitHub Actions workflow that runs the checks, when the repository has none.
    pub ci: bool,
    /// Write the files again on the branch although `.paja/checks` is on the default branch.
    pub force: bool,
    /// Don't run the detection agent: use the built-in table of known files.
    pub no_agent: bool,
}

/// What was found in the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Detected {
    /// The stacks, such as `rust` or `node (pnpm)`. Empty when none is known.
    pub stacks: Vec<String>,
    /// The commands proposed for `.paja/checks`, each with where it came from.
    pub checks: Vec<Check>,
    /// The programs proposed for `.paja/commands`.
    pub commands: Vec<String>,
    /// Where each stack lives, and the toolchains and services the checks need.
    pub environment: Environment,
    /// What the agent proposed that Paja could not back up, and why.
    pub dropped: Vec<Dropped>,
    /// Why the built-in table was used instead of the agent, when it was because the agent could
    /// not run or answered badly (not when `--no-agent` was given).
    pub fallback: Option<String>,
}

/// What `paja init` did.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub repo: String,
    pub default_branch: String,
    pub branch: String,
    pub detected: Detected,
    /// The files the pull request adds or changes, relative to the repository.
    pub files: Vec<String>,
    /// `workflow` (written), `existing` (the repository has workflows) or `none` (`.paja/ci`).
    pub ci: String,
    /// The labels created; one the repository already had is not listed.
    pub labels_created: Vec<String>,
    pub pull_request: String,
    /// Said to the person when the checks need filling in.
    pub note: Option<String>,
}

/// One stack to set up, from the agent or from the table.
struct Stack {
    name: String,
    /// Relative to the repository root; `.` for the root.
    dir: String,
    /// The manifest that shows the stack.
    file: String,
    checks: Vec<Check>,
    commands: Vec<String>,
    high_risk: Vec<String>,
    /// GitHub Actions steps (YAML, indented for a step list) that prepare the checks' tools.
    setup: String,
}

/// Table checks: the manifest backs them all.
fn table_checks(file: &str, commands: &[String]) -> Vec<Check> {
    commands
        .iter()
        .map(|command| Check {
            command: command.clone(),
            source: Source::Table,
            file: Some(file.to_string()),
        })
        .collect()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// The built-in table: the stacks whose manifest files are in `root`. The agent is not limited
/// to these; this is the fallback, and what the `--new` skeletons use.
fn detect_stacks(root: &Path) -> Vec<Stack> {
    let has = |name: &str| root.join(name).is_file();
    let mut stacks = Vec::new();
    if has("Cargo.toml") {
        stacks.push(Stack {
            name: "rust".into(),
            dir: ".".into(),
            file: "Cargo.toml".into(),
            checks: table_checks(
                "Cargo.toml",
                &strings(&[
                    "cargo fmt --all --check",
                    "cargo clippy --all-targets -- -D warnings",
                    "cargo test",
                ]),
            ),
            commands: vec!["cargo".into()],
            high_risk: strings(&[
                "Cargo.toml",
                "Cargo.lock",
                ".cargo/",
                "rust-toolchain",
                "rustfmt.toml",
                ".rustfmt.toml",
                "clippy.toml",
                ".clippy.toml",
                "build.rs",
            ]),
            setup: String::new(),
        });
    }
    if has("package.json") {
        let (pm, install) = if has("pnpm-lock.yaml") {
            (
                "pnpm",
                "      - run: corepack enable\n      - run: pnpm install --frozen-lockfile\n",
            )
        } else if has("yarn.lock") {
            ("yarn", "      - run: yarn install --frozen-lockfile\n")
        } else if has("package-lock.json") {
            ("npm", "      - run: npm ci\n")
        } else {
            ("npm", "      - run: npm install\n")
        };
        let text = std::fs::read_to_string(root.join("package.json")).unwrap_or_default();
        let scripts = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("scripts").cloned())
            .unwrap_or_default();
        let script = |name: &str| scripts.get(name).and_then(|s| s.as_str());
        let mut checks = Vec::new();
        if script("test").is_some_and(|s| !s.contains("no test specified")) {
            checks.push(format!("{pm} test"));
        }
        if script("lint").is_some() {
            checks.push(format!("{pm} run lint"));
        }
        stacks.push(Stack {
            name: format!("node ({pm})"),
            dir: ".".into(),
            file: "package.json".into(),
            checks: table_checks("package.json", &checks),
            commands: vec![pm.into()],
            high_risk: strings(&[
                "package.json",
                "package-lock.json",
                "pnpm-lock.yaml",
                "yarn.lock",
                ".npmrc",
            ]),
            setup: format!(
                "      - uses: actions/setup-node@v4\n        with:\n          node-version: lts/*\n{install}"
            ),
        });
    }
    if has("pyproject.toml") || has("requirements.txt") {
        let ruff = has("ruff.toml")
            || std::fs::read_to_string(root.join("pyproject.toml"))
                .is_ok_and(|t| t.contains("[tool.ruff"));
        let mut checks = vec!["python -m pytest".to_string()];
        let mut commands = vec!["python".to_string()];
        if ruff {
            checks.push("ruff check .".into());
            commands.push("ruff".into());
        }
        let install = if has("requirements.txt") {
            "pip install -r requirements.txt"
        } else {
            "pip install ."
        };
        let file = if has("pyproject.toml") {
            "pyproject.toml"
        } else {
            "requirements.txt"
        };
        stacks.push(Stack {
            name: "python".into(),
            dir: ".".into(),
            file: file.into(),
            checks: table_checks(file, &checks),
            commands,
            high_risk: strings(&[
                "pyproject.toml",
                "requirements.txt",
                "setup.py",
                "setup.cfg",
                "tox.ini",
            ]),
            setup: format!(
                "      - uses: actions/setup-python@v5\n        with:\n          python-version: \"3.x\"\n      \
                 - run: {install} pytest{}\n",
                if ruff { " ruff" } else { "" }
            ),
        });
    }
    if has("go.mod") {
        stacks.push(Stack {
            name: "go".into(),
            dir: ".".into(),
            file: "go.mod".into(),
            checks: table_checks("go.mod", &strings(&["go vet ./...", "go test ./..."])),
            commands: vec!["go".into()],
            high_risk: strings(&["go.mod", "go.sum"]),
            setup: "      - uses: actions/setup-go@v5\n        with:\n          go-version-file: go.mod\n"
                .into(),
        });
    }
    stacks
}

fn detected(stacks: &[Stack]) -> Detected {
    let mut d = Detected::default();
    for stack in stacks {
        d.stacks.push(stack.name.clone());
        d.environment.stacks.push(StackDir {
            name: stack.name.clone(),
            dir: stack.dir.clone(),
            file: stack.file.clone(),
        });
        for check in &stack.checks {
            if !d.checks.iter().any(|c| c.command == check.command) {
                d.checks.push(check.clone());
            }
        }
        for command in &stack.commands {
            if !d.commands.contains(command) {
                d.commands.push(command.clone());
            }
        }
    }
    d
}

fn checks_file(d: &Detected) -> String {
    let mut text = String::from(
        "# Vasara's checks for Paja: one shell command per line. Mestari reads this file from main,\n\
         # never from the branch under review.\n",
    );
    if d.checks.is_empty() {
        text.push_str(
            "# Paja found no checks it knows for this repository. Fill in the commands that must\n\
             # pass before a change lands, one per line, for example:\n\
             # make test\n",
        );
    }
    for check in &d.checks {
        text.push_str(&check.command);
        text.push('\n');
    }
    text
}

fn commands_file(d: &Detected) -> String {
    let mut text = String::from(
        "# Programs agents may run besides local git, one per line. `review <program>` lets reviewers run it too.\n",
    );
    if d.commands.is_empty() {
        text.push_str("# Paja found none it knows. List the build and test programs here, for example:\n# make\n");
    }
    for command in &d.commands {
        text.push_str(&format!("{command}\nreview {command}\n"));
    }
    text
}

fn high_risk_file(stacks: &[Stack]) -> String {
    let mut text = String::from(
        "# Path prefixes that make a change high risk: two independent reviewers must agree.\n\
         # Read from main, never from the branch under review.\n\
         # An entry that is a single name (`.github/`, `CLAUDE.md`) also matches at any depth, in any\n\
         # letter case.\n\
         .paja/\nCLAUDE.md\n.github/\n.claude/\n",
    );
    let mut seen: Vec<&str> = Vec::new();
    for entry in stacks.iter().flat_map(|s| s.high_risk.iter()) {
        if !seen.contains(&entry.as_str()) {
            seen.push(entry);
            text.push_str(entry);
            text.push('\n');
        }
    }
    text
}

/// The "Working with Paja" section: the project's commands and the rules for agents.
fn section(d: &Detected, labels: &Labels) -> String {
    let mut text = format!(
        "{SECTION}\n\n\
         Paja builds changes here from GitHub issues labelled `{}`: an agent writes the change, \
         Vasara runs the commands in `.paja/checks`, and independent reviewer agents decide whether it \
         lands.\n\n",
        labels.work
    );
    if d.checks.is_empty() {
        text.push_str("Run the project's checks (listed in `.paja/checks`) before saying a change is done.\n\n");
    } else {
        text.push_str("Run these before saying a change is done:\n\n```sh\n");
        for check in &d.checks {
            text.push_str(&check.command);
            text.push('\n');
        }
        text.push_str("```\n\n");
    }
    text.push_str(
        "Rules for agents:\n\n\
         - Keep changes small: one concern per pull request, and only what the issue asks for.\n\
         - A change that fixes a failing test must not edit that test. Do not change `.paja/` or `.github/` \
         to make a change pass.\n\
         - Never commit secrets, private links or personal details.\n\
         - Errors you add say what to do next.\n\
         - If the issue leaves a decision only a person can make, ask instead of guessing.\n",
    );
    text
}

fn claude_file(root: &Path, existing: Option<String>, d: &Detected, labels: &Labels) -> String {
    let section = section(d, labels);
    match existing {
        Some(mut text) => {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(&section);
            text
        }
        None => {
            let name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Project".into());
            format!("# {name}\n\n{section}")
        }
    }
}

fn workflow_file(stacks: &[Stack], d: &Detected) -> String {
    let mut text = String::from(
        "name: Checks\n\non:\n  pull_request:\n\njobs:\n  checks:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
    );
    for stack in stacks {
        text.push_str(&stack.setup);
    }
    for check in &d.checks {
        text.push_str(&format!("      - run: {}\n", yaml_scalar(&check.command)));
    }
    text
}

/// A plain YAML scalar when it is safe, else a single-quoted one.
fn yaml_scalar(text: &str) -> String {
    if text.contains([
        ':', '#', '\'', '"', '{', '}', '[', ']', '&', '*', '!', '|', '>', '%', '@', '`',
    ]) {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        text.to_string()
    }
}

/// Git in `dir`, trimmed stdout, or an error carrying git's stderr.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git (is it installed?)")?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_succeeds(dir: &Path, args: &[&str]) -> bool {
    git_command()
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The default branch's name and the revision to start from (`origin/<name>` when the checkout
/// has it, else the local branch).
fn default_branch(dir: &Path) -> Result<(String, String)> {
    let exists = |rev: &str| {
        git_succeeds(
            dir,
            &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")],
        )
    };
    let mut candidates = Vec::new();
    if let Ok(head) = git(
        dir,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        if let Some(name) = head.strip_prefix("origin/") {
            candidates.push(name.to_string());
        }
    }
    candidates.extend(["main".to_string(), "master".to_string()]);
    for name in candidates {
        let remote = format!("origin/{name}");
        if exists(&remote) {
            return Ok((name, remote));
        }
        if exists(&name) {
            return Ok((name.clone(), name));
        }
    }
    bail!(
        "{} has no default branch `main` or `master` with a commit; make one commit on it, push it, \
         and run `paja init` again",
        dir.display()
    )
}

/// Set the repository at `repo` (any path inside its checkout) up for Paja.
pub fn run(home: &Home, repo: &Path, options: Options) -> Result<Report> {
    let ok = ahjo::untampered(home, repo)?;
    let top = PathBuf::from(git(repo, &["rev-parse", "--show-toplevel"])?);
    let (branch, base) = default_branch(&top)?;

    if !options.force {
        let mut revs = vec![base.clone()];
        revs.push(branch.clone());
        if revs
            .iter()
            .any(|rev| git_succeeds(&top, &["cat-file", "-e", &format!("{rev}:{CHECKS_FILE}")]))
        {
            bail!(
                "{CHECKS_FILE} is already on {branch} in {}, so Paja is set up; edit the files in \
                 `.paja/` there, or run `paja init --force` to write them again on the branch {BRANCH}",
                top.display()
            );
        }
    }

    let work = home.root().join(format!("init-{}", std::process::id()));
    if work.exists() {
        std::fs::remove_dir_all(&work).with_context(|| {
            format!(
                "removing {}; delete it by hand and run again",
                work.display()
            )
        })?;
    }
    git(
        &top,
        &[
            "worktree",
            "add",
            "--no-track",
            "-B",
            BRANCH,
            &work.display().to_string(),
            &base,
        ],
    )?;
    let outcome = prepare_and_open(home, &ok, &top, &work, &branch, &base, options);
    // The branch stays; only the scratch worktree goes.
    let removed = git(
        &top,
        &["worktree", "remove", "--force", &work.display().to_string()],
    );
    let report = outcome?;
    removed.with_context(|| {
        format!(
            "the pull request is open, but removing {} failed; run `git worktree prune` after deleting it",
            work.display()
        )
    })?;
    Ok(report)
}

/// What is at `path` on `base`, a file or a directory, if anything.
fn object_kind(top: &Path, base: &str, path: &str) -> Option<detect::Kind> {
    match git(top, &["cat-file", "-t", &format!("{base}:{path}")])
        .ok()?
        .as_str()
    {
        "blob" => Some(detect::Kind::File),
        "tree" => Some(detect::Kind::Dir),
        _ => None,
    }
}

/// The agent's proposal, validated against `base`. An error means the agent could not be used.
fn ask_agent(
    home: &Home,
    top: &Path,
    work: &Path,
    base: &str,
    branch: &str,
) -> Result<detect::Accepted> {
    crate::mestari::progress("init: asking the init agent to read the repository");
    let answer = crate::mestari::init_agent(
        home,
        top,
        work,
        base,
        branch,
        &detect::prompt(base),
        &detect::schema(),
    )?;
    let proposal = detect::Proposal::parse(answer)?;
    Ok(detect::validate(proposal, &|path| {
        object_kind(top, base, path)
    }))
}

/// One line of an error, short enough to show.
fn one_line(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 300 {
        format!("{}…", line.chars().take(300).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The stacks and what goes with them: the agent's, or the table's.
fn find_stacks(
    home: &Home,
    top: &Path,
    work: &Path,
    base: &str,
    branch: &str,
    options: Options,
) -> Result<(Vec<Stack>, Detected)> {
    if options.no_agent {
        let stacks = detect_stacks(work);
        let d = detected(&stacks);
        return Ok((stacks, d));
    }
    let (accepted, why) = match ask_agent(home, top, work, base, branch) {
        Ok(accepted) if !accepted.stacks.is_empty() => (accepted, None),
        Ok(accepted) => (
            accepted,
            Some(
                "the init agent proposed no stack that the files on the default branch back up"
                    .to_string(),
            ),
        ),
        Err(err) if crate::mestari::is_fatal(&err) => return Err(err),
        Err(err) => (
            detect::Accepted::default(),
            Some(format!(
                "the init agent could not be used ({})",
                one_line(&err)
            )),
        ),
    };
    let stacks: Vec<Stack> = if why.is_some() {
        detect_stacks(work)
    } else {
        accepted
            .stacks
            .iter()
            .map(|a| Stack {
                name: a.name.clone(),
                dir: a.dir.clone(),
                file: a.file.clone(),
                checks: a.checks.clone(),
                commands: a.commands.clone(),
                high_risk: a.high_risk.clone(),
                setup: detect::render_setup(&a.setup, &a.dir, &yaml_scalar),
            })
            .collect()
    };
    let mut d = detected(&stacks);
    d.environment.toolchains = accepted.toolchains;
    d.environment.services = accepted.services;
    d.dropped = accepted.dropped;
    d.fallback = why.map(|why| {
        format!(
            "{why}; the built-in table of known files (Rust, Node, Python, Go) was used instead"
        )
    });
    Ok((stacks, d))
}

/// What the pull request body shows for review: what the agent found, each item with the file
/// that backs it, and what was dropped.
fn review_section(d: &Detected) -> String {
    let mut text = String::new();
    if d.checks.iter().any(|c| c.source == Source::Agent) {
        text.push_str("\n\nThe init agent proposed these, each backed by a file on the default branch. Review them here before merging.\n\nChecks (`.paja/checks`):\n");
        for check in &d.checks {
            match &check.file {
                Some(file) => text.push_str(&format!("- `{}` (from `{file}`)\n", check.command)),
                None => text.push_str(&format!("- `{}`\n", check.command)),
            }
        }
    }
    let env = &d.environment;
    if d.fallback.is_none()
        && (!env.stacks.is_empty() || !env.toolchains.is_empty() || !env.services.is_empty())
    {
        text.push_str("\nEnvironment:\n");
        for s in &env.stacks {
            text.push_str(&format!(
                "- stack `{}` in `{}` (`{}`)\n",
                s.name, s.dir, s.file
            ));
        }
        for t in &env.toolchains {
            text.push_str(&format!(
                "- toolchain `{} {}` (`{}`)\n",
                t.name, t.version, t.file
            ));
        }
        for s in &env.services {
            match &s.image {
                Some(image) => text.push_str(&format!(
                    "- service `{}`, image `{image}` (`{}`)\n",
                    s.name, s.file
                )),
                None => text.push_str(&format!("- service `{}` (`{}`)\n", s.name, s.file)),
            }
        }
    }
    if !d.dropped.is_empty() {
        text.push_str("\nDropped, because Paja could not back them up:\n");
        for item in &d.dropped {
            text.push_str(&format!("- {}: {}\n", item.item, item.reason));
        }
    }
    if let Some(why) = &d.fallback {
        text.push_str(&format!("\nNote: {why}.\n"));
    }
    text
}

fn prepare_and_open(
    home: &Home,
    ok: &ahjo::Untampered,
    top: &Path,
    work: &Path,
    branch: &str,
    base: &str,
    options: Options,
) -> Result<Report> {
    let labels = match std::fs::read_to_string(work.join(LABELS_FILE)) {
        Ok(text) => Labels::parse(&text)?,
        Err(_) => Labels::default(),
    };
    let (stacks, d) = find_stacks(home, top, work, base, branch, options)?;

    let mut files: Vec<(&str, String)> = vec![
        (CHECKS_FILE, checks_file(&d)),
        (COMMANDS_FILE, commands_file(&d)),
        (MODELS_FILE, MODELS.to_string()),
        (HIGH_RISK_FILE, high_risk_file(&stacks)),
    ];
    let existing_claude = std::fs::read_to_string(work.join(CLAUDE_FILE)).ok();
    // `--force` on a repository that already has the section must not add it a second time.
    if !existing_claude
        .as_deref()
        .is_some_and(|t| t.contains(SECTION))
    {
        files.push((CLAUDE_FILE, claude_file(work, existing_claude, &d, &labels)));
    }
    let has_workflows = std::fs::read_dir(work.join(".github/workflows")).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            matches!(
                e.path().extension().and_then(|x| x.to_str()),
                Some("yml" | "yaml")
            )
        })
    });
    let ci = if has_workflows {
        "existing"
    } else if options.ci && !d.checks.is_empty() {
        files.push((WORKFLOW_FILE, workflow_file(&stacks, &d)));
        "workflow"
    } else {
        files.push((CI_FILE, "none\n".to_string()));
        "none"
    };
    for (path, text) in &files {
        let target = work.join(path);
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&target, text).with_context(|| format!("writing {}", target.display()))?;
    }

    // The label first: a failure here (no permission) stops before anything is pushed.
    let mut labels_created = Vec::new();
    for (label, description) in [
        (&labels.work, "Work for Paja"),
        (&labels.follow_up, "A non-blocking finding Paja filed"),
    ] {
        match jono::gh(
            ok,
            &["label", "create", label, "--description", description],
        ) {
            Ok(_) => labels_created.push(label.clone()),
            Err(err) if err.to_string().contains("already exists") => {}
            Err(err) => return Err(err.context(format!("creating the label `{label}`"))),
        }
    }

    git(work, &["add", "-A"])?;
    if git(work, &["status", "--porcelain"])?.is_empty() {
        bail!(
            "the files `paja init` writes are already on {branch}, so there is nothing to change; \
             Paja is set up"
        );
    }
    git(
        work,
        &["commit", "-q", "-m", "Set this repository up for Paja"],
    )?;
    let mut push = vec!["push", "-q", "-u", "origin", BRANCH];
    if options.force {
        push.push("--force-with-lease");
    }
    git(work, &push).map_err(|err| {
        err.context(format!(
            "pushing {BRANCH} (never {branch}); check the `origin` remote, or run `paja init --force` if an \
             earlier {BRANCH} is on it"
        ))
    })?;
    let body = format!(
        "Sets this repository up for Paja: `.paja/checks`, `.paja/commands`, `.paja/models`, \
         `.paja/high-risk`, a `CLAUDE.md` for agents{}.\n\nDetected: {}.{}\n\nPaja starts working once \
         this is merged: queue work by labelling an issue `{}`.",
        match ci {
            "workflow" => " and a workflow that runs the checks",
            "none" => " and `.paja/ci` saying the repository has no CI",
            _ => "",
        },
        if d.stacks.is_empty() {
            "no stack Paja knows; fill in `.paja/checks`".to_string()
        } else {
            d.stacks.join(", ")
        },
        review_section(&d),
        labels.work
    );
    let out = jono::gh(
        ok,
        &[
            "pr",
            "create",
            "--base",
            branch,
            "--head",
            BRANCH,
            "--title",
            "Set this repository up for Paja",
            "--body",
            &body,
        ],
    )?;
    let mut notes = Vec::new();
    if d.checks.is_empty() {
        notes.push(format!(
            "Paja found no checks it knows: fill in {CHECKS_FILE} (and {COMMANDS_FILE}) in the pull request"
        ));
    }
    if let Some(why) = &d.fallback {
        notes.push(why.clone());
    }
    let note = (!notes.is_empty()).then(|| notes.join("; "));
    Ok(Report {
        repo: top.display().to_string(),
        default_branch: branch.to_string(),
        branch: BRANCH.to_string(),
        detected: d,
        files: files.iter().map(|(p, _)| p.to_string()).collect(),
        ci: ci.to_string(),
        labels_created,
        pull_request: out.lines().last().unwrap_or_default().trim().to_string(),
        note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_checks_follow_the_scripts_and_the_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "jest", "lint": "eslint ."}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        let d = detected(&detect_stacks(dir.path()));
        assert_eq!(d.stacks, ["node (pnpm)"]);
        let checks: Vec<&str> = d.checks.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(checks, ["pnpm test", "pnpm run lint"]);
        assert!(d.checks.iter().all(|c| c.source == Source::Table));
        assert_eq!(d.commands, ["pnpm"]);
        assert_eq!(d.environment.stacks[0].dir, ".");
    }

    #[test]
    fn npms_placeholder_test_script_is_no_check() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        )
        .unwrap();
        let d = detected(&detect_stacks(dir.path()));
        assert!(d.checks.is_empty(), "{d:?}");
        assert!(checks_file(&d).contains("Fill in"));
    }

    #[test]
    fn nothing_known_gives_commented_templates() {
        let dir = tempfile::tempdir().unwrap();
        let d = detected(&detect_stacks(dir.path()));
        assert_eq!(d, Detected::default());
        assert!(commands_file(&d).contains("# make"));
    }
}
