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
                "Paja's core: Ahjo environments (env_*) and the event log (log). \
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
