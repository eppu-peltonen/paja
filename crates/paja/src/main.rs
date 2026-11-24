use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use paja::ahjo::{self, CreateOptions, Env};
use paja::home::Home;

/// Paja: a software factory. Every command takes --json for machine-readable output.
#[derive(Parser)]
#[command(name = "paja", version)]
struct Cli {
    /// Print one JSON value on stdout instead of text; errors become {"error": "..."}.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ahjo: isolated environments (git worktrees with their own ports).
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Show the event log, newest last.
    Log {
        /// Only the last N events.
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum EnvCommand {
    /// Create an environment on a new branch `paja/<name>`.
    Create {
        name: String,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Commit, branch or tag to start from (default: the repo's HEAD).
        #[arg(long)]
        base: Option<String>,
    },
    /// List environments.
    List,
    /// Show one environment.
    Show { name: String },
    /// Remove an environment's worktree. Its branch is kept.
    Remove {
        name: String,
        /// Discard uncommitted changes in the worktree.
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            let wants_json = std::env::args().any(|a| a == "--json");
            let is_info = matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            if wants_json && !is_info {
                // Agents parse stdout, so even a usage error must be one JSON value there.
                let message = err.render().to_string();
                println!("{}", json!({ "error": message.trim() }));
                return ExitCode::from(2);
            }
            err.exit();
        }
    };
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if cli.json {
                println!("{}", json!({ "error": format!("{err:#}") }));
            } else {
                eprintln!("error: {err:#}");
            }
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let home = Home::from_env()?;
    match &cli.command {
        Command::Env { command } => match command {
            EnvCommand::Create { name, repo, base } => {
                let env = ahjo::create(
                    &home,
                    CreateOptions {
                        name,
                        repo,
                        base: base.as_deref(),
                    },
                )?;
                emit(cli, serde_json::to_value(&env)?, || describe(&env));
            }
            EnvCommand::List => {
                let envs = ahjo::list(&home)?;
                emit(cli, serde_json::to_value(&envs)?, || {
                    if envs.is_empty() {
                        "no environments".to_string()
                    } else {
                        envs.iter()
                            .map(|e| {
                                format!(
                                    "{}\t{}\tports {}-{}\t{}",
                                    e.name,
                                    e.branch,
                                    e.port_base,
                                    e.port_base + e.port_count - 1,
                                    e.path.display()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                });
            }
            EnvCommand::Show { name } => {
                let env = ahjo::get(&home, name)?;
                emit(cli, serde_json::to_value(&env)?, || describe(&env));
            }
            EnvCommand::Remove { name, force } => {
                let env = ahjo::remove(&home, name, *force)?;
                emit(
                    cli,
                    json!({ "removed": env.name, "branch_kept": env.branch }),
                    || format!("removed {} (branch {} kept)", env.name, env.branch),
                );
            }
        },
        Command::Log { limit } => {
            let mut events = home.events().read_all()?;
            if let Some(limit) = limit {
                let skip = events.len().saturating_sub(*limit);
                events.drain(..skip);
            }
            emit(cli, serde_json::to_value(&events)?, || {
                events
                    .iter()
                    .map(|e| format!("{}\t{}\t{}\t{}", e.seq, e.ts_ms, e.kind, e.data))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
        }
    }
    Ok(())
}

fn emit(cli: &Cli, value: Value, text: impl FnOnce() -> String) {
    if cli.json {
        println!("{value}");
    } else {
        let text = text();
        if !text.is_empty() {
            println!("{text}");
        }
    }
}

fn describe(env: &Env) -> String {
    format!(
        "{}\n  path:   {}\n  branch: {} (from {})\n  ports:  {}-{}",
        env.name,
        env.path.display(),
        env.branch,
        &env.base_commit[..env.base_commit.len().min(12)],
        env.port_base,
        env.port_base + env.port_count - 1
    )
}
