use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use paja::ahjo::{self, CreateOptions, Env};
use paja::home::Home;
use paja::jono;
use paja::mestari;

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
    /// Jono: the work queue, imported from GitHub issues labelled `paja`.
    Work {
        #[command(subcommand)]
        command: WorkCommand,
    },
    /// Mestari: take the next work item and run it to an outcome (landed or escalated).
    ///
    /// Builds in a fresh environment with headless Claude Code, runs the checks in
    /// `.paja/checks` on main, has independent reviewer agents read the diff, then lands it
    /// through a pull request or asks on the issue.
    Run {
        /// Any path inside the repository's checkout (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Keep going: after each item take the next, and when the queue is empty or a run
        /// fails, check again every this many minutes. Prints one JSON value per line, one
        /// per item.
        #[arg(long, value_name = "MINUTES", value_parser = clap::value_parser!(u64).range(1..))]
        every: Option<u64>,
    },
    /// The deny-list hook Mestari runs agents under (Claude Code `PreToolUse`). Reads the
    /// tool call on stdin; exits 2 to block, also on any error, and 0 to allow. Prints no
    /// JSON: Claude Code reads only its exit code and stderr.
    #[command(hide = true)]
    Hook {
        /// The agent's environment: the only place it may change files.
        #[arg(long)]
        root: Option<PathBuf>,
        /// The factory's home, which agents never touch.
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Serve the same commands as MCP tools on stdin/stdout, for Claude Code.
    ///
    /// stdout carries the MCP protocol, so this is the one command that never prints a
    /// result of its own; its errors go to stderr.
    Mcp,
    /// Show the event log, newest last.
    Log {
        /// Only the last N events.
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum WorkCommand {
    /// Import open issues labelled `paja` from the checkout's GitHub repository (uses `gh`).
    Sync {
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// List work items, oldest first.
    List,
    /// Show one work item by its issue number.
    Show { number: u64 },
    /// The next item to work on: the oldest open, queued one, or null.
    Next,
    /// Take the next item and mark it active in one step, so two runs never share one.
    Claim,
    /// Set an item's status: queued, active, landed or escalated.
    Set {
        number: u64,
        status: String,
        /// Why, for example the question an escalation asks.
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
enum EnvCommand {
    /// Create an environment on branch `paja/<name>` (resumed if an earlier one kept it).
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
            // Flags end at a bare `--`; a later "--json" is a value, not the flag.
            let json = std::env::args_os()
                .take_while(|a| a != "--")
                .any(|a| a == "--json");
            if !json {
                err.exit();
            }
            // Agents parse stdout, so help, version and usage errors are one JSON value too.
            let text = err.render().to_string();
            let text = text.trim();
            let (value, code) = match err.kind() {
                ErrorKind::DisplayHelp => (json!({ "help": text }), ExitCode::SUCCESS),
                // `paja` with no subcommand: still a usage error, so it carries `error` like
                // every other failure, plus the help text.
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => (
                    json!({ "error": "a subcommand is required", "help": text }),
                    ExitCode::from(2),
                ),
                ErrorKind::DisplayVersion => (json!({ "version": text }), ExitCode::SUCCESS),
                _ => (json!({ "error": text }), ExitCode::from(2)),
            };
            println!("{value}");
            return code;
        }
    };
    if let Command::Hook { root, home } = &cli.command {
        return run_hook(root.clone(), home.clone());
    }
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

/// Fail closed: anything but a clear "allowed" blocks the call.
fn run_hook(root: Option<PathBuf>, home: Option<PathBuf>) -> ExitCode {
    // A panic would exit 101, which Claude Code treats as "allow"; block instead.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("Blocked: Paja's deny-list hook crashed ({info}), so it fails closed");
        std::process::exit(2);
    }));
    let decision = (|| -> Result<Option<String>> {
        let bounds = paja::hook::Bounds::new(root, home)?;
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let input: paja::hook::HookInput = serde_json::from_str(&input)?;
        paja::hook::check(&input, &bounds)
    })();
    match decision {
        Ok(None) => ExitCode::SUCCESS,
        Ok(Some(reason)) => {
            eprintln!("Blocked by Paja's deny-list: {reason}");
            ExitCode::from(2)
        }
        Err(err) => {
            eprintln!(
                "Blocked: Paja's deny-list hook couldn't decide ({err:#}), so it fails closed"
            );
            ExitCode::from(2)
        }
    }
}

fn run(cli: &Cli) -> Result<()> {
    let home = Home::from_env()?;
    match &cli.command {
        Command::Hook { .. } => unreachable!("handled before the home is opened"),
        Command::Mcp => {
            // Once the session is up, stdout is the protocol: report a later failure on
            // stderr so it can't be read as a frame.
            if let Err(err) = paja::mcp::serve(home) {
                eprintln!("error: {err:#}");
                std::process::exit(1);
            }
        }
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
                    serde_json::to_value(ahjo::RemoveReport::from(&env))?,
                    || format!("removed {} (branch {} kept)", env.name, env.branch),
                );
            }
        },
        Command::Run { repo, every } => loop {
            let outcome = mestari::run_once(&home, mestari::RunOptions { repo });
            let Some(minutes) = every else {
                let report = outcome?;
                emit(cli, serde_json::to_value(&report)?, || {
                    describe_run(&report)
                });
                break;
            };
            // A long-running loop reports an error and carries on; it must not stop the factory.
            let idle = match outcome {
                Ok(report) => {
                    emit(cli, serde_json::to_value(&report)?, || {
                        describe_run(&report)
                    });
                    matches!(report, mestari::RunReport::Idle)
                }
                Err(err) => {
                    emit(cli, json!({ "error": format!("{err:#}") }), || {
                        format!("error: {err:#}")
                    });
                    true
                }
            };
            if idle {
                std::thread::sleep(std::time::Duration::from_secs(minutes.saturating_mul(60)));
            }
        },
        Command::Work { command } => match command {
            WorkCommand::Sync { repo } => {
                let report = jono::sync(&home, repo)?;
                emit(cli, serde_json::to_value(&report)?, || {
                    format!(
                        "imported {:?}, changed {:?}, closed {:?}, reopened {:?}",
                        report.imported, report.changed, report.closed, report.reopened
                    )
                });
            }
            WorkCommand::List => {
                let items = jono::list(&home)?;
                emit(cli, serde_json::to_value(&items)?, || {
                    if items.is_empty() {
                        "no work items; run `paja work sync`".to_string()
                    } else {
                        items
                            .iter()
                            .map(describe_item)
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                });
            }
            WorkCommand::Show { number } => {
                let item = jono::get(&home, *number)?;
                emit(cli, serde_json::to_value(&item)?, || {
                    format!("{}\n  {}\n\n{}", describe_item(&item), item.url, item.body)
                });
            }
            WorkCommand::Claim => {
                let item = jono::claim(&home)?;
                emit(cli, serde_json::to_value(&item)?, || {
                    item.as_ref()
                        .map_or("nothing queued".to_string(), describe_item)
                });
            }
            WorkCommand::Set {
                number,
                status,
                note,
            } => {
                let status = jono::parse_status(status)?;
                let item = jono::set_status(&home, *number, status, note.as_deref())?;
                emit(cli, serde_json::to_value(&item)?, || describe_item(&item));
            }
            WorkCommand::Next => {
                let item = jono::next(&home)?;
                emit(cli, serde_json::to_value(&item)?, || {
                    item.as_ref()
                        .map_or("nothing queued".to_string(), describe_item)
                });
            }
        },
        Command::Log { limit } => {
            let events = home.events().tail(*limit)?;
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

fn describe_run(report: &mestari::RunReport) -> String {
    match report {
        mestari::RunReport::Idle => "nothing queued".to_string(),
        mestari::RunReport::Landed { number, pr, rounds } => {
            format!("#{number} landed in {rounds} round(s): {pr}")
        }
        mestari::RunReport::Escalated {
            number, question, ..
        } => format!("#{number} escalated:\n{question}"),
    }
}

fn describe_item(item: &jono::WorkItem) -> String {
    let mut line = format!(
        "#{}\t{}{}\t{}",
        item.number,
        item.status.as_str(),
        if item.open { "" } else { " (issue closed)" },
        item.title
    );
    if let Some(note) = &item.note {
        line.push_str(&format!("\n  note: {note}"));
    }
    line
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
