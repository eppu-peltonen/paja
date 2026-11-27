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
