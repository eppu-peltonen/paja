//! `paja init --new`: a new GitHub repository with a walking skeleton for a stack.
//!
//! The skeleton is a minimal app with one passing test, a formatter and a linter, a workflow that
//! runs the checks on pull requests, and the `.paja/` files and `CLAUDE.md` that `paja init` writes
//! for the stack. The templates are in the binary, so only GitHub needs the network. The first
//! commit goes straight to the default branch: the repository is empty and new, the one place Paja
//! pushes there.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::*;

/// The stacks `--new` has a skeleton for.
pub const STACKS: [&str; 3] = ["rust", "node", "python"];

/// What `paja init --new` was asked for.
#[derive(Debug, Clone)]
pub struct NewOptions {
    pub name: String,
    pub stack: String,
    /// A private repository (the default) or a public one.
    pub private: bool,
    /// Where to clone it; `<name>` in the current directory when missing.
    pub dir: Option<PathBuf>,
}

/// What `paja init --new` did.
#[derive(Debug, Clone, Serialize)]
pub struct NewReport {
    /// `owner/name`.
    pub repo: String,
    pub url: String,
    pub private: bool,
    /// The clone, an absolute path.
    pub directory: String,
    pub stack: String,
    pub default_branch: String,
    /// The files of the first commit, relative to the directory.
    pub files: Vec<String>,
    pub checks: Vec<String>,
    pub labels_created: Vec<String>,
    pub next_steps: Vec<String>,
}

/// A skeleton file: its path in the repository, and its template (`{{name}}` and `{{crate}}`
/// are replaced).
type Template = (&'static str, &'static str);

const RUST: &[Template] = &[
    (
        "Cargo.toml",
        include_str!("../../templates/rust/Cargo.toml.tmpl"),
    ),
    (
        "src/main.rs",
        include_str!("../../templates/rust/main.rs.tmpl"),
    ),
    (
        "rustfmt.toml",
        include_str!("../../templates/rust/rustfmt.toml.tmpl"),
    ),
    (
        ".gitignore",
        include_str!("../../templates/rust/gitignore.tmpl"),
    ),
];

const NODE: &[Template] = &[
    (
        "package.json",
        include_str!("../../templates/node/package.json.tmpl"),
    ),
    (
        "index.js",
        include_str!("../../templates/node/index.js.tmpl"),
    ),
    (
        "index.test.js",
        include_str!("../../templates/node/index.test.js.tmpl"),
    ),
    (
        "eslint.config.js",
        include_str!("../../templates/node/eslint.config.js.tmpl"),
    ),
    (
        ".prettierrc.json",
        include_str!("../../templates/node/prettierrc.json.tmpl"),
    ),
    (
        ".gitignore",
        include_str!("../../templates/node/gitignore.tmpl"),
    ),
];

const PYTHON: &[Template] = &[
    (
        "pyproject.toml",
        include_str!("../../templates/python/pyproject.toml.tmpl"),
    ),
    (
        "requirements.txt",
        include_str!("../../templates/python/requirements.txt.tmpl"),
    ),
    ("app.py", include_str!("../../templates/python/app.py.tmpl")),
    (
        "test_app.py",
        include_str!("../../templates/python/test_app.py.tmpl"),
    ),
    (
        ".gitignore",
        include_str!("../../templates/python/gitignore.tmpl"),
    ),
];

/// The labels a new project names for its queue.
const LABELS: &str = "\
# The labels Paja's queue uses: `work` puts an issue in the queue, `follow-up` marks the
# non-blocking findings Paja files. Read from the default branch.
work paja
follow-up paja-follow-up
";

fn skeleton(stack: &str) -> Result<&'static [Template]> {
    match stack {
        "rust" => Ok(RUST),
        "node" => Ok(NODE),
        "python" => Ok(PYTHON),
        _ => bail!("no skeleton for the stack {stack:?}; use --stack rust, node or python"),
    }
}

/// A project name safe as a repository name, a directory and a package name.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 100
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok {
        bail!(
            "{name:?} is not a usable project name; use letters, digits, `-`, `_` and `.`, starting \
             with a letter or digit, for example `paja init --new my-app --stack rust`"
        );
    }
    // GitHub drops a `.git` ending, so the repository and the folder would differ.
    if name.to_ascii_lowercase().ends_with(".git") {
        bail!(
            "{name:?} ends in `.git`, which GitHub drops from a repository name; use {:?}",
            &name[..name.len() - 4]
        );
    }
    Ok(())
}

/// Lowercase letters and digits with `-` between, for package names.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn render(template: &str, name: &str) -> String {
    let slug = slug(name);
    let mut krate = slug.replace('-', "_");
    if krate.starts_with(|c: char| c.is_ascii_digit()) {
        krate.insert_str(0, "app_");
    }
    template
        .replace("{{name}}", &slug)
        .replace("{{crate}}", &krate)
}

/// `gh` in `dir`, trimmed stdout, or an error carrying its stderr.
fn gh(dir: &Path, args: &[&str]) -> Result<String> {
    let gh = jono::gh_program();
    let output =
        crate::platform::own_process_group(Command::new(&gh).env("GH_PROMPT_DISABLED", "1"))
            .current_dir(dir)
            .args(args)
            .output()
            .with_context(|| {
                format!("running `{gh}` (install the GitHub CLI and run `gh auth login`)")
            })?;
    if !output.status.success() {
        bail!(
            "`{gh} {} {}` failed: {}; check `gh auth status`",
            args[0],
            args[1],
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Write `text` to `target`, creating its folders first.
fn write_new(target: &Path, text: &str) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(target, text).with_context(|| format!("writing {}", target.display()))
}

/// Create the repository on GitHub, clone it, write the skeleton, and push the first commit.
pub fn run_new(home: &Home, cwd: &Path, options: NewOptions) -> Result<NewReport> {
    let NewOptions {
        name,
        stack,
        private,
        dir,
    } = options;
    check_name(&name)?;
    let templates = skeleton(&stack)?;
    let dir = match dir {
        Some(dir) => std::path::absolute(cwd.join(dir))?,
        None => cwd.join(&name),
    };
    if dir.exists() && std::fs::read_dir(&dir).is_ok_and(|mut e| e.next().is_some()) {
        bail!(
            "{} exists and is not empty; pick another --dir, or remove it first",
            dir.display()
        );
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let visibility = if private { "--private" } else { "--public" };
    let created = gh(cwd, &["repo", "create", &name, visibility])?;
    let url = created
        .lines()
        .rev()
        .find(|l| l.trim().starts_with("http"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if url.is_empty() {
        bail!("`gh repo create {name}` printed no repository URL: {created:?}; check `gh auth status`");
    }
    let repo = {
        let mut parts = url.trim_end_matches('/').rsplit('/');
        let name = parts.next().unwrap_or_default();
        format!("{}/{name}", parts.next().unwrap_or_default())
    };
    let after_create = |err: anyhow::Error| {
        err.context(format!(
            "the repository {url} was created; finish by hand in {} or delete it on GitHub",
            dir.display()
        ))
    };
    gh(cwd, &["repo", "clone", &url, &dir.display().to_string()]).map_err(after_create)?;

    let ok = ahjo::untampered(home, &dir).map_err(after_create)?;
    for (path, template) in templates {
        write_new(&dir.join(path), &render(template, &name)).map_err(after_create)?;
    }
    let stacks = detect_stacks(&dir);
    let mut d = detected(&stacks);
    if stack == "python" {
        d.checks.push("ruff format --check .".into());
    }
    let labels = Labels::parse(LABELS)?;

    let mut files: Vec<(String, String)> = vec![
        (CHECKS_FILE.into(), checks_file(&d)),
        (COMMANDS_FILE.into(), commands_file(&d)),
        (MODELS_FILE.into(), MODELS.into()),
        (HIGH_RISK_FILE.into(), high_risk_file(&stacks)),
        (LABELS_FILE.into(), LABELS.into()),
        (CLAUDE_FILE.into(), claude_file(&dir, None, &d, &labels)),
        (WORKFLOW_FILE.into(), workflow_file(&stacks, &d)),
    ];
    for (path, text) in &files {
        write_new(&dir.join(path), text).map_err(after_create)?;
    }
    let mut names: Vec<String> = templates.iter().map(|(p, _)| p.to_string()).collect();
    names.extend(files.drain(..).map(|(p, _)| p));
    names.sort();

    let mut labels_created = Vec::new();
    for (label, description) in [
        (&labels.work, "Work for Paja"),
        (&labels.follow_up, "A non-blocking finding Paja filed"),
    ] {
        match jono::gh(
            &ok,
            &["label", "create", label, "--description", description],
        ) {
            Ok(_) => labels_created.push(label.clone()),
            Err(err) if err.to_string().contains("already exists") => {}
            Err(err) => {
                return Err(after_create(
                    err.context(format!("creating the label `{label}`")),
                ))
            }
        }
    }

    git(&dir, &["add", "-A"]).map_err(after_create)?;
    git(
        &dir,
        &["commit", "-q", "-m", "Start the project from a walking skeleton"],
    )
    .map_err(|err| {
        after_create(err.context(
            "committing the skeleton; set `user.name` and `user.email` in git, then commit and push by hand",
        ))
    })?;
    let default_branch = git(&dir, &["symbolic-ref", "--short", "HEAD"]).map_err(after_create)?;
    git(&dir, &["push", "-q", "-u", "origin", &default_branch]).map_err(after_create)?;

    Ok(NewReport {
        repo,
        url,
        private,
        directory: dir.display().to_string(),
        stack,
        default_branch,
        files: names,
        checks: d.checks,
        labels_created,
        next_steps: vec![
            format!(
                "cd {} and file an issue with /paja:spec (it gets the `{}` label)",
                dir.display(),
                labels.work
            ),
            "run `paja run --once`".into(),
        ],
    })
}
