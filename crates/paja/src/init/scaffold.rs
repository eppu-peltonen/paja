//! `paja init --new`: a new GitHub repository with a walking skeleton for a stack.
//!
//! The skeleton is a minimal app with one passing test, a formatter and a linter, a workflow that
//! runs the checks on pull requests, and the `.paja/` files and `CLAUDE.md` that `paja init` writes
//! for the stack. For rust, node and python the templates are in the binary, so only GitHub needs
//! the network; any other stack gets a skeleton an agent writes (see `generate`), and the first
//! commit waits until its checks pass here. The first commit goes straight to the default branch:
//! the repository is empty and new, the one place Paja pushes there.

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
    /// `built-in` (Paja's template for rust, node or python) or `generated` (an agent wrote it).
    pub skeleton: String,
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
        _ => bail!("no built-in skeleton for the stack {stack:?}; Paja generates one instead"),
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
    // A Windows checkout may embed the templates with CRLF; Prettier and ESLint want LF.
    template
        .replace("\r\n", "\n")
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

/// Have the init agent write the skeleton in the fresh clone `dir`, then detect its stacks and
/// checks the way `paja init` does. The files are staged and a commit object made for the
/// detection to read; no branch points at it, so nothing is committed.
fn generated(
    home: &Home,
    dir: &Path,
    name: &str,
    stack: &str,
    programs: &[String],
    default_branch: &str,
) -> Result<(Vec<Stack>, Detected)> {
    crate::mestari::progress(&format!(
        "init: asking the init agent to write a {stack} skeleton"
    ));
    crate::mestari::skeleton_agent(
        home,
        dir,
        programs,
        &generate::skeleton_prompt(name, stack, programs),
    )?;
    if git(dir, &["status", "--porcelain"])?.is_empty() {
        bail!(
            "the init agent wrote no files for the {stack} skeleton; to avoid it, use a stack with \
             a built-in skeleton (rust, node, python), which needs no init agent"
        );
    }
    // Paja writes these from what it finds; the agent's own copies would only disagree.
    let paja_dir = dir.join(".paja");
    if paja_dir.exists() {
        std::fs::remove_dir_all(&paja_dir)
            .with_context(|| format!("removing {}", paja_dir.display()))?;
    }
    git(dir, &["add", "-A"])?;
    let tree = git(dir, &["write-tree"])?;
    let base = git(
        dir,
        &[
            "-c",
            "user.name=paja",
            "-c",
            "user.email=paja@localhost",
            "commit-tree",
            &tree,
            "-m",
            "skeleton",
        ],
    )?;
    let (stacks, d) = find_stacks(home, dir, dir, &base, default_branch, Options::default())?;
    if d.checks.is_empty() {
        bail!(
            "no check could be found for the generated {stack} skeleton (a formatter, a linter and \
             a test command that files in it back up), so nothing was verified and nothing was \
             pushed; write `.paja/checks` by hand in the clone, then commit and push"
        );
    }
    Ok((stacks, d))
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
    let builtin = recommend::skeleton_of(&stack) == "built-in";
    let stack = if builtin {
        stack.trim().to_ascii_lowercase()
    } else {
        generate::check_stack(&stack)?
    };
    let templates: &[Template] = if builtin { skeleton(&stack)? } else { &[] };
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

    // Before anything is created: no programs to allow means no skeleton, and nothing to clean up.
    let programs = if builtin {
        Vec::new()
    } else {
        crate::mestari::progress(&format!(
            "init: asking the init agent which programs a {stack} skeleton needs"
        ));
        let answer = crate::mestari::recommend_agent(
            home,
            &generate::programs_prompt(&stack),
            &generate::programs_schema(),
        )?;
        generate::parse_programs(answer, &stack)?
    };

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
            "the repository {url} was created; finish by hand in {dir}, or delete the new \
             repository on GitHub (`gh repo delete {repo}`, which needs the `delete_repo` scope: \
             `gh auth refresh -s delete_repo`) and remove {dir} before running `paja init --new` \
             again with the same name",
            dir = dir.display()
        ))
    };
    gh(cwd, &["repo", "clone", &url, &dir.display().to_string()]).map_err(after_create)?;

    let ok = ahjo::untampered(home, &dir).map_err(after_create)?;
    let default_branch = git(&dir, &["symbolic-ref", "--short", "HEAD"]).map_err(after_create)?;
    for (path, template) in templates {
        write_new(&dir.join(path), &render(template, &name)).map_err(after_create)?;
    }
    let (stacks, d) = if builtin {
        let stacks = detect_stacks(&dir);
        let mut d = detected(&stacks);
        if stack == "python" {
            d.checks.push(Check {
                command: "ruff format --check .".into(),
                source: Source::Table,
                file: Some("pyproject.toml".into()),
            });
        }
        (stacks, d)
    } else {
        generated(home, &dir, &name, &stack, &programs, &default_branch).map_err(after_create)?
    };
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
    git(&dir, &["add", "-A"]).map_err(after_create)?;
    let names: Vec<String> = if builtin {
        let mut names: Vec<String> = templates.iter().map(|(p, _)| p.to_string()).collect();
        names.extend(files.drain(..).map(|(p, _)| p));
        names.sort();
        names
    } else {
        let mut names: Vec<String> = git(&dir, &["ls-files"])
            .map_err(after_create)?
            .lines()
            .map(str::to_string)
            .collect();
        names.sort();
        names
    };

    // The commit takes what is staged now, so what the checks write is never committed.
    if !builtin {
        let commands: Vec<String> = d.checks.iter().map(|c| c.command.clone()).collect();
        crate::mestari::progress("init: running the checks on the generated skeleton");
        let failed =
            crate::mestari::skeleton_checks(home, &dir, &commands).map_err(after_create)?;
        if let Some(failure) = failed {
            return Err(after_create(anyhow::anyhow!(
                "the generated {stack} skeleton fails its own check: {failure}\n\nNothing was \
                 committed or pushed; fix it in the clone, then commit and push by hand"
            )));
        }
    }

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

    git(
        &dir,
        &["commit", "-q", "-m", "Start the project from a walking skeleton"],
    )
    .map_err(|err| {
        after_create(err.context(
            "committing the skeleton; set `user.name` and `user.email` in git, then commit and push by hand",
        ))
    })?;
    git(&dir, &["push", "-q", "-u", "origin", &default_branch]).map_err(after_create)?;

    Ok(NewReport {
        repo,
        url,
        private,
        directory: dir.display().to_string(),
        stack,
        skeleton: if builtin { "built-in" } else { "generated" }.to_string(),
        default_branch,
        files: names,
        checks: d.checks.into_iter().map(|c| c.command).collect(),
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

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn render_gives_lf_endings_even_from_a_crlf_template() {
        let out = render("{\r\n  \"name\": \"{{name}}\"\r\n}\r\n", "My_App");
        assert_eq!(out, "{\n  \"name\": \"my-app\"\n}\n");
        assert!(!out.contains('\r'));
    }
}
