//! The MCP server: the same core functions as the CLI, as tools over stdio.
//!
//! Claude Code starts `paja mcp` from the project directory, so a tool's `repo` defaults to the
//! server's working directory, as `--repo .` does for the CLI. Each tool returns the same JSON
//! as the matching `paja --json` command; a failure is a tool error whose text is
//! `{"error": "..."}`, so an agent reads both the same way.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ahjo::{self, CreateOptions};
use crate::home::Home;
use crate::jono;

#[derive(Clone)]
pub struct PajaMcp {
    home: Home,
    #[allow(dead_code)] // read by the code #[tool_handler] generates
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CreateArgs {
    /// Environment name: 1-40 characters of a-z, 0-9 and '-'. The branch is `paja/<name>`.
    name: String,
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
    /// Commit, branch or tag to start from. Defaults to the repository's HEAD.
    base: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NameArgs {
    /// Environment name.
    name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RemoveArgs {
    /// Environment name.
    name: String,
    /// Discard uncommitted changes in the worktree.
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RepoArgs {
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
    /// List the work items of every repository, each with its repository.
    #[serde(default)]
    all: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct NumberArgs {
    /// The GitHub issue number, in the repository of the checkout.
    number: u64,
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SetArgs {
    /// The GitHub issue number, in the repository of the checkout.
    number: u64,
    /// queued, active, landed or escalated.
    status: String,
    /// Why, for example the question an escalation asks.
    note: Option<String>,
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AnswerArgs {
    /// The GitHub issue number of an escalated item.
    number: u64,
    /// The answer: recorded for the next run and posted as a comment on the issue.
    text: String,
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct FileArgs {
    /// The issue's title.
    title: String,
    /// The issue's body: what to build and the acceptance criteria.
    #[serde(default)]
    body: String,
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct InitArgs {
    /// Any path inside the repository. Defaults to the directory the server runs in.
    repo: Option<String>,
    /// When the repository has no workflow, write one that runs the checks on pull requests.
    #[serde(default)]
    ci: bool,
    /// Write the files again on the branch although .paja/checks is on the default branch.
    #[serde(default)]
    force: bool,
    /// Start a new project instead: the name of a GitHub repository to create under the
    /// signed-in account. Needs `stack`; `repo`, `ci` and `force` are not used.
    new: Option<String>,
    /// With `new`: rust, node or python.
    stack: Option<String>,
    /// With `new`: make the repository public (default: private).
    #[serde(default)]
    public: bool,
    /// With `new`: the directory to clone into (default: the name, in the server's directory).
    dir: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LogArgs {
    /// Only the last N events.
    limit: Option<usize>,
}

/// The checkout a tool acts on: `repo`, or the server's directory. Absolute, so errors name the
/// real directory rather than `.`.
fn checkout(repo: Option<String>) -> Result<PathBuf> {
    Ok(std::path::absolute(PathBuf::from(
        repo.unwrap_or_else(|| ".".into()),
    ))?)
}

/// Run blocking core work (git, the home lock) off the async runtime, and shape the result
/// the way `paja --json` prints it.
async fn call(work: impl FnOnce() -> Result<Value> + Send + 'static) -> Result<String, String> {
    let outcome = tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| anyhow::anyhow!("the tool stopped unexpectedly: {err}"))
        .and_then(|r| r);
    match outcome {
        Ok(value) => Ok(value.to_string()),
        Err(err) => Err(json!({ "error": format!("{err:#}") }).to_string()),
    }
}

#[tool_router]
impl PajaMcp {
    pub fn new(home: Home) -> Self {
        Self {
            home,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Ahjo: create an isolated environment, a git worktree on branch paja/<name> with its own block of 100 ports. A branch kept from an earlier environment of the same name is resumed."
    )]
    async fn env_create(&self, Parameters(args): Parameters<CreateArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            // Absolute, so errors name the real directory rather than `.`.
            let repo = std::path::absolute(PathBuf::from(args.repo.unwrap_or_else(|| ".".into())))?;
            let env = ahjo::create(
                &home,
                CreateOptions {
                    name: &args.name,
                    repo: &repo,
                    base: args.base.as_deref(),
                },
            )?;
            Ok(serde_json::to_value(env)?)
        })
        .await
    }

    #[tool(description = "Ahjo: list environments.")]
    async fn env_list(&self) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(ahjo::list(&home)?)?)).await
    }

    #[tool(description = "Ahjo: show one environment: its path, branch, base commit and ports.")]
    async fn env_show(&self, Parameters(args): Parameters<NameArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(ahjo::get(&home, &args.name)?)?)).await
    }

    #[tool(
        description = "Ahjo: remove an environment's worktree. Its branch is kept. Refuses uncommitted changes unless force is true."
    )]
    async fn env_remove(&self, Parameters(args): Parameters<RemoveArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let env = ahjo::remove(&home, &args.name, args.force)?;
            Ok(serde_json::to_value(ahjo::RemoveReport::from(&env))?)
        })
        .await
    }

    #[tool(
        description = "Jono: import open GitHub issues labelled paja into the work queue (uses gh)."
    )]
    async fn work_sync(&self, Parameters(args): Parameters<RepoArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let repo = std::path::absolute(PathBuf::from(args.repo.unwrap_or_else(|| ".".into())))?;
            Ok(serde_json::to_value(jono::sync(&home, &repo)?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: list the work items of the checkout's repository, oldest first, or of every repository with all."
    )]
    async fn work_list(&self, Parameters(args): Parameters<ListArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let checkout = checkout(args.repo)?;
            let items = if args.all {
                jono::list_all_from(&home, &checkout)?
            } else {
                jono::list(&home, &jono::repo_name(&home, &checkout)?)?
            };
            Ok(serde_json::to_value(items)?)
        })
        .await
    }

    #[tool(
        description = "Jono: show one work item of the checkout's repository by its issue number."
    )]
    async fn work_show(&self, Parameters(args): Parameters<NumberArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let name = jono::repo_name(&home, &checkout(args.repo)?)?;
            Ok(serde_json::to_value(jono::get(&home, &name, args.number)?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: the next item to work on in the checkout's repository (the oldest queued one), or null."
    )]
    async fn work_next(&self, Parameters(args): Parameters<RepoArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let name = jono::repo_name(&home, &checkout(args.repo)?)?;
            Ok(serde_json::to_value(jono::next(&home, &name)?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: take the next item of the checkout's repository and mark it active in one step, so two runs never share one. Null when nothing is queued."
    )]
    async fn work_claim(&self, Parameters(args): Parameters<RepoArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let name = jono::repo_name(&home, &checkout(args.repo)?)?;
            Ok(serde_json::to_value(jono::claim(&home, &name)?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: set an item's status (queued, active, landed or escalated), with an optional note. An escalated item can't be set to queued: use work_answer, so the next run gets the answer."
    )]
    async fn work_set(&self, Parameters(args): Parameters<SetArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let status = jono::parse_status(&args.status)?;
            let name = jono::repo_name(&home, &checkout(args.repo)?)?;
            Ok(serde_json::to_value(jono::set_status(
                &home,
                &name,
                args.number,
                status,
                args.note.as_deref(),
            )?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: answer an escalated item. Records the answer for Paja's next run (its agents get the question and answer) and queues the item again, then posts the answer as a comment on its GitHub issue (uses gh)."
    )]
    async fn work_answer(
        &self,
        Parameters(args): Parameters<AnswerArgs>,
    ) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let repo = std::path::absolute(PathBuf::from(args.repo.unwrap_or_else(|| ".".into())))?;
            Ok(serde_json::to_value(jono::answer(
                &home,
                &repo,
                args.number,
                &args.text,
            )?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: file a new GitHub issue labelled paja (uses gh), for example an approved spec. The next sync queues it. Returns its url."
    )]
    async fn work_file(&self, Parameters(args): Parameters<FileArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let repo = std::path::absolute(PathBuf::from(args.repo.unwrap_or_else(|| ".".into())))?;
            Ok(serde_json::to_value(jono::file(
                &home,
                &repo,
                &args.title,
                &args.body,
            )?)?)
        })
        .await
    }

    #[tool(
        description = "Set an existing repository up for Paja in one pull request: detects the stack, writes .paja/checks, commands, models and high-risk, adds CLAUDE.md (or a Working with Paja section), creates the work label (uses gh), pushes the branch paja/init and opens a pull request to the default branch. Refuses when .paja/checks is already on the default branch, unless force is true. With new (a repository name) and stack (rust, node or python) it instead creates that GitHub repository, clones it (to dir, or the name) and pushes a walking skeleton as the first commit on the default branch, returning {repo, url, directory, files, next_steps}."
    )]
    async fn init(&self, Parameters(args): Parameters<InitArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            if let Some(name) = args.new {
                let Some(stack) = args.stack else {
                    anyhow::bail!("`new` needs `stack`: rust, node or python");
                };
                let report = crate::init::run_new(
                    &home,
                    &std::env::current_dir()?,
                    crate::init::NewOptions {
                        name,
                        stack,
                        private: !args.public,
                        dir: args.dir.map(PathBuf::from),
                    },
                )?;
                return Ok(serde_json::to_value(report)?);
            }
            let report = crate::init::run(
                &home,
                &checkout(args.repo)?,
                crate::init::Options {
                    ci: args.ci,
                    force: args.force,
                },
            )?;
            Ok(serde_json::to_value(report)?)
        })
        .await
    }

    #[tool(
        description = "Check that this machine and the repository are ready for paja run: git, gh, claude, the deny-list hook, the repository's labels and .paja files, and the sandbox. Returns {checks: [{name, status (ok, warning or failed), detail, fix}], ok}."
    )]
    async fn doctor(&self, Parameters(args): Parameters<RepoArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            Ok(serde_json::to_value(crate::doctor::run_checks(
                &home,
                &checkout(args.repo)?,
            )?)?)
        })
        .await
    }

    #[tool(description = "Show the event log, oldest first.")]
    async fn log(&self, Parameters(args): Parameters<LogArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(home.events().tail(args.limit)?)?)).await
    }
}

#[tool_handler]
impl ServerHandler for PajaMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("paja", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Paja's core: Ahjo environments (env_*), Jono's work queue (work_*) and the event log (log). \
                 Every result is the JSON that `paja --json` prints.",
            )
    }
}

/// Serve MCP on stdin and stdout until the client disconnects.
pub fn serve(home: Home) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let service = PajaMcp::new(home)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|err| anyhow::anyhow!("starting the MCP server on stdio: {err}"))?;
        service
            .waiting()
            .await
            .map_err(|err| anyhow::anyhow!("MCP server stopped: {err}"))?;
        Ok(())
    })
}
