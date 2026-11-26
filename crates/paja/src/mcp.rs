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
pub struct NumberArgs {
    /// The GitHub issue number.
    number: u64,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SetArgs {
    /// The GitHub issue number.
    number: u64,
    /// queued, active, landed or escalated.
    status: String,
    /// Why, for example the question an escalation asks.
    note: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct AnswerArgs {
    /// The GitHub issue number of an escalated item.
    number: u64,
    /// The answer, posted as a comment on the issue.
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
pub struct LogArgs {
    /// Only the last N events.
    limit: Option<usize>,
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

    #[tool(description = "Jono: list work items, oldest first.")]
    async fn work_list(&self) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(jono::list(&home)?)?)).await
    }

    #[tool(description = "Jono: show one work item by its issue number.")]
    async fn work_show(&self, Parameters(args): Parameters<NumberArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(jono::get(&home, args.number)?)?)).await
    }

    #[tool(description = "Jono: the next item to work on (the oldest queued one), or null.")]
    async fn work_next(&self) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(jono::next(&home)?)?)).await
    }

    #[tool(
        description = "Jono: take the next item and mark it active in one step, so two runs never share one. Null when nothing is queued."
    )]
    async fn work_claim(&self) -> Result<String, String> {
        let home = self.home.clone();
        call(move || Ok(serde_json::to_value(jono::claim(&home)?)?)).await
    }

    #[tool(
        description = "Jono: set an item's status (queued, active, landed or escalated), with an optional note. An escalated item can't be set to queued: use work_answer, so the next run gets the answer."
    )]
    async fn work_set(&self, Parameters(args): Parameters<SetArgs>) -> Result<String, String> {
        let home = self.home.clone();
        call(move || {
            let status = jono::parse_status(&args.status)?;
            Ok(serde_json::to_value(jono::set_status(
                &home,
                args.number,
                status,
                args.note.as_deref(),
            )?)?)
        })
        .await
    }

    #[tool(
        description = "Jono: answer an escalated item. Posts the answer as a comment on its GitHub issue (uses gh) and queues the item again."
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
