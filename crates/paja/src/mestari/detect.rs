//! `paja init`'s detection agent: one read-only session on the `init` role that reads a checkout
//! of the default branch and answers with the stacks, checks, commands and environment it finds.
//! The deny-list hook and the read-only tools are the same as the triage agent's; `paja init`
//! validates what comes back (see `init::detect`).

use super::*;

/// Run the init agent in `work`, a checkout of `base` (the default branch's tip) that Paja made
/// for it, and return its `structured_output`. `repo` is the repository's top level; `branch` the
/// default branch's name, for messages about `.paja/models`.
pub fn init_agent(
    home: &Home,
    repo: &Path,
    work: &Path,
    base: &str,
    branch: &str,
    prompt: &str,
    schema: &Value,
) -> Result<Value> {
    // An unreadable `.paja/models` must not stop a repository being set up: its roles run on the
    // account's default model until the file is fixed.
    let models = Models::parse_on(
        &read_from_base(repo, base, MODELS_FILE)?.unwrap_or_default(),
        branch,
    )
    .unwrap_or_default();
    let env = Env {
        name: "init".to_string(),
        repo: repo.to_path_buf(),
        path: work.to_path_buf(),
        branch: crate::init::BRANCH.to_string(),
        base_commit: base.to_string(),
        port_base: 0,
        port_count: 0,
        created_ms: 0,
    };
    crate::sandbox::activate(start_sandbox(home, repo, base)?);
    let result = (|| {
        let tools = [
            "Read",
            "Glob",
            "Grep",
            "Bash(git status*)",
            "Bash(git diff*)",
            "Bash(git log*)",
            "Bash(git show*)",
            "Bash(tail *)",
            "Bash(head *)",
            "Bash(grep *)",
            "Bash(wc *)",
            "Bash(pwd)",
        ];
        let out = claude(
            home,
            &env,
            base,
            prompt,
            &tools,
            Some(schema),
            true,
            &models.args("init"),
        )?;
        out.get("structured_output")
            .cloned()
            .context("the init agent returned no answer")
    })();
    crate::sandbox::activate(None);
    result
}

/// Run the init agent for `paja init --recommend`: no repository to read, so it starts in an empty
/// scratch repository (made under the home's `envs` folder, which the sandbox can reach, and
/// removed afterwards) with no file or command it may use, and answers from the prompt alone.
pub fn recommend_agent(home: &Home, prompt: &str, schema: &Value) -> Result<Value> {
    let envs = home.envs_dir();
    let dir = envs.join(format!("recommend-{}", std::process::id()));
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| {
            format!(
                "removing {}; delete it by hand and run again",
                dir.display()
            )
        })?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let result = (|| {
        git(&dir, &["init", "-q"])?;
        git(
            &dir,
            &[
                "-c",
                "user.name=paja",
                "-c",
                "user.email=paja@localhost",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "empty",
            ],
        )?;
        let base = git(&dir, &["rev-parse", "HEAD"])?;
        let env = Env {
            name: "init".to_string(),
            repo: dir.clone(),
            path: dir.clone(),
            branch: git(&dir, &["symbolic-ref", "--short", "HEAD"])?,
            base_commit: base.clone(),
            port_base: 0,
            port_count: 0,
            created_ms: 0,
        };
        crate::sandbox::activate(start_sandbox(home, &dir, &base)?);
        let out = claude(
            home,
            &env,
            &base,
            prompt,
            // Nothing is pre-approved but a command that prints the folder: no file to read, no
            // other command.
            &["Bash(pwd)"],
            Some(schema),
            true,
            &Models::default().args("init"),
        );
        crate::sandbox::activate(None);
        out?.get("structured_output")
            .cloned()
            .context("the init agent returned no answer")
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}
