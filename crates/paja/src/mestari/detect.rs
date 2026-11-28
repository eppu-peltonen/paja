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
        // The repository's CLAUDE.md is data the agent reads and judges, not its rules, so it
        // isn't put in the system prompt (where a large one could also overrun the command line).
        let out = claude_with(
            home,
            &env,
            base,
            prompt,
            &tools,
            Some(schema),
            true,
            &models.args("init"),
            None,
            Harness::None,
        )?;
        out.get("structured_output")
            .cloned()
            .context("the init agent returned no answer")
    })();
    crate::sandbox::activate(None);
    result
}

/// A sandbox, when `PAJA_SBX` asks for one, that can reach the fresh clone `dir`, which is not
/// under the home's `envs` folder.
fn clone_sandbox(home: &Home, dir: &Path) -> Result<Option<crate::sandbox::Sbx>> {
    if crate::sandbox::configured().is_none() {
        progress(&format!(
            "no sandbox ({} is not set): agents and checks run as you, behind the deny-list hook only",
            crate::sandbox::PROGRAM_VAR
        ));
        return Ok(None);
    }
    progress("starting the sandbox");
    let envs = home.envs_dir();
    std::fs::create_dir_all(&envs).with_context(|| format!("creating {}", envs.display()))?;
    crate::sandbox::start(&[envs, dir.to_path_buf()], None)
}

/// Run the skeleton agent in `dir`, a fresh clone with no commit, and wait for it to finish: it
/// writes the files and leaves them uncommitted. Nothing in the clone is on a default branch yet,
/// so the agent's harness and `.paja/commands` come from an empty commit made for it (no branch
/// points at it), and `programs`, the stack's tools, are what it may run besides local git.
pub fn skeleton_agent(home: &Home, dir: &Path, programs: &[String], prompt: &str) -> Result<()> {
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
            "empty",
        ],
    )?;
    let env = Env {
        name: "init".to_string(),
        repo: dir.to_path_buf(),
        path: dir.to_path_buf(),
        branch: git(dir, &["symbolic-ref", "--short", "HEAD"])?,
        base_commit: base.clone(),
        port_base: 0,
        port_count: 0,
        created_ms: 0,
    };
    crate::sandbox::activate(clone_sandbox(home, dir)?);
    let result = {
        let program_tools = program_tools(programs);
        let mut tools: Vec<&str> = vec!["Read", "Edit", "Write", "Glob", "Grep"];
        tools.extend(program_tools.iter().map(String::as_str));
        tools.extend([
            "Bash(git status*)",
            "Bash(git diff*)",
            "Bash(git log*)",
            "Bash(git show*)",
            "Bash(tail *)",
            "Bash(head *)",
            "Bash(grep *)",
            "Bash(wc *)",
            "Bash(pwd)",
        ]);
        claude_with(
            home,
            &env,
            &base,
            prompt,
            &tools,
            None,
            false,
            &Models::default().args("init"),
            Some(programs),
            Harness::DefaultBranch,
        )
    };
    crate::sandbox::activate(None);
    result?;
    // The agent may run local git, but the skeleton must stay uncommitted on the clone's branch:
    // a commit would hide its files from the next step, and another branch would push nothing.
    let branch = git(dir, &["symbolic-ref", "--short", "HEAD"]).unwrap_or_default();
    let unborn = git(dir, &["rev-parse", "--verify", "-q", "HEAD"]).is_err();
    if branch != env.branch || !unborn {
        bail!(
            "the init agent committed or switched branches in {} (it must leave the skeleton \
             uncommitted on `{}`), so nothing was pushed; delete the new repository on GitHub \
             (`gh repo delete`) and that folder, then run `paja init --new` again, or use a stack \
             with a built-in skeleton (rust, node, python)",
            dir.display(),
            env.branch
        );
    }
    Ok(())
}

/// Run `checks` in `dir` the way Vasara runs them, and return the first failure's command and
/// output, or None when they all pass.
pub fn skeleton_checks(home: &Home, dir: &Path, checks: &[String]) -> Result<Option<String>> {
    let guard = GitGuard::new(dir)?;
    crate::sandbox::activate(clone_sandbox(home, dir)?);
    let result = run_checks(dir, checks);
    crate::sandbox::activate(None);
    guard.check("Vasara's checks")?;
    result
}

/// The scratch repository's folder name for one `recommend_agent` call: the process id and a
/// count, so two calls at once in one long-lived `paja mcp` never share (and wipe) one folder.
fn recommend_scratch_name() -> String {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("recommend-{}-{n}", std::process::id())
}

/// Run the init agent for `paja init --recommend`: no repository to read, so it starts in an empty
/// scratch repository (made under the home's `envs` folder, which the sandbox can reach, and
/// removed afterwards) with no file or command it may use, and answers from the prompt alone.
pub fn recommend_agent(home: &Home, prompt: &str, schema: &Value) -> Result<Value> {
    let envs = home.envs_dir();
    let dir = envs.join(recommend_scratch_name());
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

#[cfg(test)]
mod tests {
    #[test]
    fn each_recommend_call_gets_its_own_scratch_folder() {
        let (a, b) = (
            super::recommend_scratch_name(),
            super::recommend_scratch_name(),
        );
        assert_ne!(a, b);
        let pid = format!("recommend-{}-", std::process::id());
        assert!(a.starts_with(&pid) && b.starts_with(&pid), "{a} {b}");
    }
}
