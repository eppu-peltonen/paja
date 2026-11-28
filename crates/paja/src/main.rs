use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use paja::ahjo::{self, CreateOptions, Env};
use paja::home::Home;
use paja::jono;
use paja::mestari;

/// The version and the commit this binary was built from.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("PAJA_COMMIT"), ")");

/// Paja: a software factory. Every command takes --json for machine-readable output.
#[derive(Parser)]
#[command(name = "paja", version = VERSION)]
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
    /// Jono: the work queue, imported from GitHub issues with the work label (`.paja/labels`).
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
        /// Take one item, run it to landed or escalated, and exit. The default; the first
        /// command to try.
        #[arg(long, conflicts_with = "every")]
        once: bool,
        /// Take this item instead of the oldest queued one.
        #[arg(long, value_name = "NUMBER", conflicts_with = "every")]
        item: Option<u64>,
        /// Don't ask for confirmation, even on a terminal. Not with --every, which never asks.
        #[arg(long, conflicts_with = "every")]
        yes: bool,
        /// After Paja stopped because git's configuration changed during a run: you have
        /// checked and restored the files it named, and Paja may run git in this repository
        /// again.
        #[arg(long)]
        tamper_checked: bool,
        /// Read `stop` (stop after this step) or `stop now` lines on stdin, as the first and
        /// second Ctrl-C do. For a program that starts runs without a terminal, such as Paja
        /// Desk; never asks for confirmation.
        #[arg(long)]
        stop_on_stdin: bool,
        /// With --every: keep up to this many items going at once, each in a lane of its own
        /// (its own `paja run` and environment). `paja pool lanes <N>` changes the number while
        /// it runs, and `paja pool status` shows the lanes.
        #[arg(long, value_name = "N", requires = "every", value_parser = clap::value_parser!(u32).range(1..=paja::mestari::pool::MAX_LANES as i64))]
        workers: Option<u32>,
        /// A pool's lane: its pool synced Jono already, so the run doesn't.
        #[arg(long, hide = true, requires = "item")]
        lane: bool,
    },
    /// The worker pool of `paja run --workers`: its lane count and its lanes.
    Pool {
        #[command(subcommand)]
        command: PoolCommand,
    },
    /// Sort the open follow-up issues: one cheap agent reading the default branch marks each as
    /// keep, duplicate, already fixed or not worth doing. Duplicates and already-fixed issues
    /// are closed with a comment giving the reason and the evidence; "not worth doing" is only
    /// listed, for you to decide. `paja run --every` does this once a day when the queue is idle.
    Triage {
        /// Any path inside the repository's checkout (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Set an existing repository up for Paja in one pull request.
    ///
    /// An agent detects the stacks, checks and environment (any language); Paja keeps what the
    /// files on the default branch back up. Writes `.paja/checks`, `.paja/commands`,
    /// `.paja/models` and `.paja/high-risk`, adds a `CLAUDE.md` (or a "Working with Paja" section to the existing
    /// one), creates the work label, and opens a pull request from `paja/init`. Never pushes to
    /// the default branch; Paja starts working once the pull request is merged.
    Init {
        /// Any path inside the repository's checkout (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// When the repository has no GitHub Actions workflow, write one that runs the checks on
        /// pull requests. Without it, `.paja/ci` says `none`.
        #[arg(long, conflicts_with = "new")]
        ci: bool,
        /// Write the files again on the branch although `.paja/checks` is on the default branch.
        #[arg(long, conflicts_with = "new")]
        force: bool,
        /// Don't run the detection agent: use the built-in table of known files (Rust, Node,
        /// Python, Go). The agent also steps aside when `claude` is missing or not signed in.
        #[arg(long, conflicts_with = "new")]
        no_agent: bool,
        /// Start a new project instead: create this GitHub repository under the signed-in
        /// account, clone it, and push a walking skeleton (a minimal app with a passing test, a
        /// formatter, a linter, a CI workflow, and the `.paja/` files and `CLAUDE.md`) as the
        /// first commit on the default branch. Without --stack, on a terminal, it asks what you
        /// are building and which language or framework you prefer, shows recommended stacks, and
        /// takes a number or a stack name. With --json or no terminal, --stack is required.
        #[arg(long, value_name = "NAME", conflicts_with = "recommend")]
        new: Option<String>,
        /// The skeleton's stack, with --new: any language or framework. rust, node and python
        /// have a built-in skeleton; an agent generates one for any other, and nothing is pushed
        /// unless its checks pass. For a stack to choose, see --recommend.
        #[arg(long, requires = "new")]
        stack: Option<String>,
        /// Recommend stacks for a project that doesn't exist yet instead of setting one up: an
        /// agent reads this description (and any --prefer) and answers with one to three options,
        /// best first, each with why, trade-offs and whether its skeleton is `built-in` (rust,
        /// node, python) or `generated`. Changes nothing.
        #[arg(
            long,
            value_name = "WHAT YOU ARE BUILDING",
            conflicts_with_all = ["ci", "force", "no_agent"]
        )]
        recommend: Option<String>,
        /// With --recommend: a language or framework you prefer; repeat it for several. It wins
        /// unless it is a poor fit for what you are building.
        #[arg(long, requires = "recommend")]
        prefer: Vec<String>,
        /// With --new: make the repository public (the default is private).
        #[arg(long, requires = "new", conflicts_with = "private")]
        public: bool,
        /// With --new: make the repository private (the default).
        #[arg(long, requires = "new")]
        private: bool,
        /// With --new: clone into this directory instead of `<name>` here.
        #[arg(long, requires = "new")]
        dir: Option<PathBuf>,
    },
    /// Check that this machine and repository are ready for `paja run`: git, `gh`, `claude`,
    /// the deny-list hook, the repository's labels and `.paja` files, and the sandbox. Each
    /// check is ok, a warning or failed, with its fix; the exit code is non-zero when any failed.
    Doctor {
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
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
        /// A file the hook touches on every call, so Mestari can tell it really ran.
        #[arg(long)]
        stamp: Option<PathBuf>,
        /// For reviewers: block every change, inside the worktree too.
        #[arg(long)]
        read_only: bool,
        /// The programs besides local git the agent may run, comma-terminated (`cargo,npm,`;
        /// `,` for none). Without it the hook blocks every call.
        #[arg(long)]
        programs: Option<String>,
    },
    /// Serve the same commands as MCP tools on stdin/stdout, for Claude Code.
    ///
    /// stdout carries the MCP protocol, so this is the one command that never prints a
    /// result of its own; its errors go to stderr.
    Mcp,
    /// What each work item and each day spent: rounds, steps by role and model, tokens, turns,
    /// wall time and outcome per item; totals per day and overall; the median and 90th
    /// percentile per landed item. Steps whose usage couldn't be read are counted as unknown.
    Usage {
        /// Any path inside the repository's checkout (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Only this work item.
        #[arg(long, value_name = "NUMBER")]
        item: Option<u64>,
        /// Only steps recorded on or after this UTC date (YYYY-MM-DD).
        #[arg(long, value_name = "DATE")]
        since: Option<String>,
    },
    /// Show the event log, newest last.
    Log {
        /// Only the last N events.
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
enum PoolCommand {
    /// Set the number of lanes the repository's pool keeps busy, or show it. A running pool
    /// reads it on its next tick: more lanes start then; fewer let running items finish and
    /// start no new ones until the busy lanes are under the number.
    Lanes {
        /// 0 to 32; 0 lets the busy lanes finish and starts none. Leave it out to show it.
        #[arg(value_parser = clap::value_parser!(u32).range(0..=paja::mestari::pool::MAX_LANES as i64))]
        lanes: Option<u32>,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Which files a work item will likely touch, which the pool admits it by: its `Touches:`
    /// line, else what a short read-only planning agent (the `plan` role) finds on the default
    /// branch. Recorded as `pool.planned`.
    Plan {
        /// The item's issue number.
        number: u64,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Read `stop now` lines on stdin, as a pool's planner does: the planning agent is
        /// ended with every process it started, and no plan is recorded. Stdin closing stops it
        /// the same way.
        #[arg(long)]
        stop_on_stdin: bool,
    },
    /// The pool: running or stopped, its lanes (item, stage, step, time in step, spend, files),
    /// the items waiting and why, and the latest landings.
    Status {
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorkCommand {
    /// Import open issues with the work label from the checkout's GitHub repository (uses `gh`),
    /// and remove the clean environment of a parked item whose issue was closed (the branch stays).
    Sync {
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// List the checkout's repository's work items, oldest first.
    List {
        /// List every repository's items, each with its repository.
        #[arg(long)]
        all: bool,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Show one work item by its issue number.
    Show {
        number: u64,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// The next item to work on: the oldest open, queued one, or null.
    Next {
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Take the next item and mark it active in one step, so two runs never share one.
    Claim {
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Answer an escalated item: comment the answer on its issue and queue it again.
    Answer {
        number: u64,
        /// What Paja should do, posted as a comment on the issue.
        text: String,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// File a new GitHub issue with the work label (uses `gh`); the next sync queues it.
    File {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
    },
    /// Set an item's status: queued, active, landed or escalated.
    Set {
        number: u64,
        status: String,
        /// Why, for example the question an escalation asks.
        #[arg(long)]
        note: Option<String>,
        /// Any path inside the repository (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
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
    // Before any thread: a pool's choice for its lane's backend, never passed on further.
    paja::sandbox::take_passed();
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
                ErrorKind::DisplayVersion => (
                    json!({
                        "version": env!("CARGO_PKG_VERSION"),
                        "commit": env!("PAJA_COMMIT"),
                    }),
                    ExitCode::SUCCESS,
                ),
                _ => (json!({ "error": text }), ExitCode::from(2)),
            };
            paja::platform::print_line(format_args!("{value}"));
            return code;
        }
    };
    if let Command::Hook {
        root,
        home,
        stamp,
        read_only,
        programs,
    } = &cli.command
    {
        return paja::hook::run(
            root.clone(),
            home.clone(),
            stamp.clone(),
            *read_only,
            programs.clone(),
        );
    }
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            if cli.json {
                paja::platform::print_line(json!({ "error": format!("{err:#}") }));
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
        Command::Hook { .. } => unreachable!("handled before the home is opened"),
        Command::Mcp => {
            // Once the session is up, stdout is the protocol: report a later failure on
            // stderr so it can't be read as a frame.
            if let Err(err) = paja::mcp::serve(home) {
                eprintln!("error: {err:#}");
                std::process::exit(1);
            }
        }
        Command::Init {
            recommend: Some(building),
            prefer,
            ..
        } => {
            let found = paja::init::recommend(&home, building, prefer)?;
            emit(cli, serde_json::to_value(&found)?, || {
                describe_recommendations(&found)
            });
        }
        Command::Init {
            new: Some(name),
            stack,
            public,
            dir,
            ..
        } => {
            let stack = match stack {
                Some(stack) => stack.clone(),
                None => {
                    let stdin = std::io::stdin();
                    if cli.json || !std::io::IsTerminal::is_terminal(&stdin) {
                        anyhow::bail!(
                            "`paja init --new {name}` needs --stack (a language or framework, such as \
                             rust, node, python or go) here; run \
                             `paja init --recommend \"<what you are building>\"` first to get \
                             options, then pass the one you pick as --stack"
                        );
                    }
                    paja::init::choose_stack(
                        &mut stdin.lock(),
                        &mut std::io::stderr(),
                        &mut |building, prefer| paja::init::recommend(&home, building, prefer),
                    )?
                }
            };
            let report = paja::init::run_new(
                &home,
                &std::env::current_dir()?,
                paja::init::NewOptions {
                    name: name.clone(),
                    stack,
                    private: !*public,
                    dir: dir.clone(),
                },
            )?;
            emit(cli, serde_json::to_value(&report)?, || {
                describe_new(&report)
            });
        }
        Command::Init {
            repo,
            ci,
            force,
            no_agent,
            ..
        } => {
            let report = paja::init::run(
                &home,
                &std::path::absolute(repo)?,
                paja::init::Options {
                    ci: *ci,
                    force: *force,
                    no_agent: *no_agent,
                },
            )?;
            emit(cli, serde_json::to_value(&report)?, || {
                describe_init(&report)
            });
        }
        Command::Triage { repo } => {
            paja::platform::catch_interrupts()?;
            let report = mestari::triage(&home, repo)?;
            emit(cli, serde_json::to_value(&report)?, || {
                describe_triage(&report)
            });
        }
        Command::Doctor { repo } => {
            let report = paja::doctor::run_checks(&home, repo)?;
            emit(cli, serde_json::to_value(&report)?, || {
                describe_doctor(&report)
            });
            if !report.ok {
                std::process::exit(1);
            }
        }
        Command::Env { command } => match command {
            EnvCommand::Create { name, repo, base } => {
                let env = ahjo::create_by_person(
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
        Command::Run {
            repo,
            every,
            once,
            item,
            yes,
            tamper_checked,
            stop_on_stdin,
            workers,
            lane,
        } => {
            // Only a single run on a terminal asks; a loop and a script never wait for a person.
            // `--once` is also what a run without `--every` does; the flag names it.
            let _ = once;
            let confirm = every.is_none()
                && !*yes
                && !*stop_on_stdin
                && (std::io::IsTerminal::is_terminal(&std::io::stdin())
                    // Debug builds only: lets tests answer the question with no terminal.
                    || (cfg!(debug_assertions) && std::env::var_os("PAJA_TEST_ANSWER").is_some()));
            // The person's word that the repository was checked counts once, for the first
            // pass, not for every later stop of a long-running loop.
            let mut checked = *tamper_checked;
            let mut failures = paja::posti::Failures::default();
            // Ctrl-C finishes the step in flight and stops; a second one stops at once.
            paja::platform::catch_interrupts()?;
            if *stop_on_stdin {
                paja::platform::stop_on_stdin(if *lane {
                    paja::platform::StopFrom::Pool
                } else {
                    paja::platform::StopFrom::Program
                });
            }
            if let (Some(lanes), Some(minutes)) = (workers, every) {
                let pool = mestari::pool::run(
                    &home,
                    mestari::pool::PoolOptions {
                        repo,
                        lanes: *lanes,
                        every: *minutes,
                        json: cli.json,
                    },
                );
                if let Err(err) = &pool {
                    if mestari::is_fatal(err) {
                        paja::posti::stopped(&home, repo);
                    }
                }
                return pool;
            }
            loop {
                let outcome = mestari::run_once(
                    &home,
                    mestari::RunOptions {
                        repo,
                        tamper_checked: std::mem::take(&mut checked),
                        item: *item,
                        confirm,
                        sync: !*lane,
                    },
                );
                let Some(minutes) = every else {
                    let pass = match outcome {
                        Ok(pass) => pass,
                        Err(err) => {
                            if mestari::is_fatal(&err) {
                                paja::posti::stopped(&home, repo);
                            } else if !mestari::is_mistake(&err) {
                                // A mistake in the command is in front of the person already.
                                paja::posti::pass_failed(&home, repo, false);
                            }
                            return Err(err);
                        }
                    };
                    emit(cli, run_json(&pass)?, || describe_run(&pass.report));
                    break;
                };
                // A long-running loop reports an error and carries on; it must not stop the factory.
                let mut idle = false;
                // A usage limit waits for its reset instead of polling the queue.
                let mut reset: Option<u64> = None;
                let wait = match outcome {
                    Ok(pass) => {
                        failures.passed();
                        emit(cli, run_json(&pass)?, || describe_run(&pass.report));
                        let report = pass.report;
                        idle = matches!(report, mestari::RunReport::Idle);
                        reset = match &report {
                            mestari::RunReport::Paused { resets_at } => Some(*resets_at),
                            mestari::RunReport::UsageLimit { resets_at, .. } => *resets_at,
                            _ => None,
                        };
                        match &report {
                            mestari::RunReport::Idle => Some("nothing to do right now"),
                            mestari::RunReport::Paused { .. } => {
                                Some("Claude Code's usage limit hasn't reset")
                            }
                            mestari::RunReport::UsageLimit { .. } => {
                                Some("Claude Code's usage limit was hit")
                            }
                            _ => None,
                        }
                    }
                    // Except when git's configuration was tampered with (running git again is the
                    // danger) or no later pass could start either: the loop stops for a person.
                    Err(err) if mestari::is_fatal(&err) => {
                        paja::posti::stopped(&home, repo);
                        return Err(err);
                    }
                    Err(err) => {
                        emit(cli, json!({ "error": format!("{err:#}") }), || {
                            format!("error: {err:#}")
                        });
                        if failures.failed() {
                            paja::posti::pass_failed(&home, repo, true);
                        }
                        Some("that pass failed; Paja keeps going")
                    }
                };
                if paja::platform::interrupts() > 0 {
                    break;
                }
                // An idle queue is the time for the daily triage of the follow-up issues.
                if idle {
                    match mestari::triage_if_due(&home, repo) {
                        Ok(Some(report)) => emit(
                            cli,
                            json!({ "outcome": "triage", "triage": report }),
                            || describe_triage(&report),
                        ),
                        Ok(None) => {}
                        Err(err) if mestari::is_fatal(&err) => {
                            paja::posti::stopped(&home, repo);
                            return Err(err);
                        }
                        Err(err) => emit(cli, json!({ "error": format!("{err:#}") }), || {
                            format!("error: {err:#}")
                        }),
                    }
                    if paja::platform::interrupts() > 0 {
                        break;
                    }
                }
                if let Some(why) = wait {
                    let until = match reset {
                        Some(ms) => {
                            let ahead = ms.saturating_sub(paja::events::now_ms());
                            mestari::progress(&format!(
                                "{why}; waiting until it resets at {} (Ctrl+C stops)",
                                mestari::format_utc(ms)
                            ));
                            std::time::Instant::now() + std::time::Duration::from_millis(ahead)
                        }
                        None => {
                            mestari::progress(&format!(
                                "{why}; checking again in {minutes} minute{} (Ctrl+C stops)",
                                if *minutes == 1 { "" } else { "s" }
                            ));
                            std::time::Instant::now()
                                + std::time::Duration::from_secs(minutes.saturating_mul(60))
                        }
                    };
                    while std::time::Instant::now() < until && paja::platform::interrupts() == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    if paja::platform::interrupts() > 0 {
                        break;
                    }
                }
            }
        }
        Command::Work { command } => match command {
            WorkCommand::Sync { repo } => {
                let report = jono::sync(&home, repo)?;
                emit(cli, serde_json::to_value(&report)?, || {
                    let mut text = format!(
                        "imported {:?}, changed {:?}, closed {:?}, reopened {:?}",
                        report.imported, report.changed, report.closed, report.reopened
                    );
                    if !report.freed.is_empty() {
                        text.push_str(&format!("\nfreed {}", report.freed.join(", ")));
                    }
                    for kept in &report.kept {
                        text.push_str(&format!("\nkept {}: {}", kept.env, kept.why));
                    }
                    text
                });
            }
            WorkCommand::List { all, repo } => {
                let checkout = std::path::absolute(repo)?;
                let items = if *all {
                    jono::list_all_from(&home, &checkout)?
                } else {
                    jono::list(&home, &jono::repo_name(&home, &checkout)?)?
                };
                emit(cli, serde_json::to_value(&items)?, || {
                    // The text puts what waits on the person first; the JSON keeps Jono's order.
                    let mut items = items.clone();
                    items.sort_by_key(|item| item.status != jono::Status::Escalated);
                    if items.is_empty() {
                        "no work items; run `paja work sync`".to_string()
                    } else if *all {
                        // Each repository's items on their own, with its repository first.
                        items
                            .iter()
                            .map(|item| format!("{}\t{}", item.repo, describe_item(item)))
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        items
                            .iter()
                            .map(describe_item)
                            .chain(jono::cycle_note(&items))
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                });
            }
            WorkCommand::Show { number, repo } => {
                let name = jono::repo_name(&home, &std::path::absolute(repo)?)?;
                let shown = jono::show(&home, &name, *number)?;
                let item: jono::WorkItem = serde_json::from_value(shown.clone())?;
                emit(cli, shown.clone(), || {
                    let run = describe_live(&shown["run"])
                        .map(|r| format!("\n  {r}"))
                        .unwrap_or_default();
                    format!(
                        "{}{run}\n  {}\n\n{}",
                        describe_item(&item),
                        item.url,
                        item.body
                    )
                });
            }
            WorkCommand::Claim { repo } => {
                let name = jono::repo_name(&home, &std::path::absolute(repo)?)?;
                let item = jono::claim(&home, &name)?;
                emit(cli, serde_json::to_value(&item)?, || {
                    item.as_ref()
                        .map_or("nothing queued".to_string(), describe_item)
                });
            }
            WorkCommand::Set {
                number,
                status,
                note,
                repo,
            } => {
                let status = jono::parse_status(status)?;
                let checkout = std::path::absolute(repo)?;
                let item = mestari::set_status(&home, &checkout, *number, status, note.as_deref())?;
                emit(cli, serde_json::to_value(&item)?, || describe_item(&item));
            }
            WorkCommand::Answer { number, text, repo } => {
                let item = jono::answer(&home, &std::path::absolute(repo)?, *number, text)?;
                emit(cli, serde_json::to_value(&item)?, || describe_item(&item));
            }
            WorkCommand::File { title, body, repo } => {
                let filed = jono::file(&home, &std::path::absolute(repo)?, title, body)?;
                emit(cli, serde_json::to_value(&filed)?, || filed.url.clone());
            }
            WorkCommand::Next { repo } => {
                let name = jono::repo_name(&home, &std::path::absolute(repo)?)?;
                let item = jono::next(&home, &name)?;
                emit(cli, serde_json::to_value(&item)?, || {
                    item.as_ref()
                        .map_or("nothing queued".to_string(), describe_item)
                });
            }
        },
        Command::Pool { command } => match command {
            PoolCommand::Lanes { lanes, repo } => {
                let name = jono::repo_name(&home, &std::path::absolute(repo)?)?;
                let report = mestari::pool::lanes(&home, &name, *lanes)?;
                emit(cli, serde_json::to_value(&report)?, || {
                    let count = report
                        .lanes
                        .map_or("no lane count set".to_string(), |n| format!("{n} lanes"));
                    let state = if report.running {
                        "running"
                    } else {
                        "not running"
                    };
                    format!("pool of {}: {count} ({state})", report.repo)
                });
            }
            PoolCommand::Plan {
                number,
                repo,
                stop_on_stdin,
            } => {
                if *stop_on_stdin {
                    // Caught, a signal or console event (a Ctrl-Break on Windows, a hangup) stops
                    // the planner as `stop now` does, ending its agent, instead of killing the
                    // planner and leaving the agent going. The hint is set first, so even the
                    // first one stops it now.
                    paja::platform::stop_on_stdin(paja::platform::StopFrom::PoolPlanner);
                    paja::platform::catch_interrupts()?;
                }
                let plan = mestari::plan::plan(&home, repo, *number)?;
                emit(cli, serde_json::to_value(&plan)?, || {
                    format!(
                        "#{} touches ({}{}): {}",
                        plan.number,
                        plan.source,
                        if plan.high_risk { ", high risk" } else { "" },
                        plan.paths.join(", ")
                    )
                });
            }
            PoolCommand::Status { repo } => {
                let name = jono::repo_name(&home, &std::path::absolute(repo)?)?;
                let status = mestari::pool::status(&home, &name)?;
                emit(cli, serde_json::to_value(&status)?, || {
                    mestari::pool::describe(&status, paja::events::now_ms())
                });
            }
        },
        Command::Usage { repo, item, since } => {
            let name = jono::repo_name(&home, &std::path::absolute(repo)?)?;
            let report = paja::usage::report(&home, &name, *item, since.as_deref())?;
            emit(cli, paja::usage::to_json(&report)?, || {
                paja::usage::describe(&report)
            });
        }
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
        paja::platform::print_line(format_args!("{value}"));
    } else {
        let text = text();
        if !text.is_empty() {
            paja::platform::print_line(format_args!("{text}"));
        }
    }
}

/// The run's report, with whether a sandbox was in use.
fn run_json(pass: &mestari::Pass) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(&pass.report)?;
    if let Some(fields) = value.as_object_mut() {
        fields.insert("sandbox".to_string(), json!(pass.sandbox));
    }
    Ok(value)
}

fn describe_doctor(report: &paja::doctor::Report) -> String {
    let mut out = Vec::new();
    for check in &report.checks {
        out.push(format!(
            "{:<8}{}: {}",
            check.status.as_str(),
            check.name,
            check.detail
        ));
        if let Some(fix) = &check.fix {
            out.push(format!("        fix: {fix}"));
        }
    }
    out.push(if report.ok {
        "ready: no check failed".to_string()
    } else {
        "not ready: fix the failed checks above, then run `paja doctor` again".to_string()
    });
    out.push(format!("more on errors and fixes: {}", report.help));
    out.join("\n")
}

fn describe_triage(report: &mestari::TriageReport) -> String {
    use mestari::Verdict;
    let notes = report.retried_notes.iter().map(|note| match &note.error {
        None => format!(
            "noted on #{} that #{} was closed as its duplicate",
            note.on, note.number
        ),
        Some(error) => format!(
            "noting on #{} that #{} was closed as its duplicate failed again: {error}",
            note.on, note.number
        ),
    });
    if report.items.is_empty() {
        let mut lines = vec!["no open follow-up issues to triage".to_string()];
        lines.extend(notes);
        return lines.join("\n");
    }
    let mut lines: Vec<String> = report
        .items
        .iter()
        .map(|item| {
            let what = match (item.verdict, item.duplicate_of) {
                (Verdict::Keep, _) => "keep".to_string(),
                (Verdict::Duplicate, Some(of)) => format!("duplicate of #{of}"),
                (Verdict::Duplicate, None) => "duplicate".to_string(),
                (Verdict::AlreadyFixed, _) => "already fixed".to_string(),
                (Verdict::NotWorthDoing, _) => "not worth doing".to_string(),
            };
            let state = match (&item.error, item.closed) {
                // A closed item's error is its note's, which says it was closed.
                (Some(error), true) => format!(" ({error})"),
                (Some(error), false) => format!(" (not closed: {error})"),
                (None, true) => " (closed)".to_string(),
                (None, false) => String::new(),
            };
            format!(
                "#{} {}: {what}{state}\n    {}",
                item.number, item.title, item.reason
            )
        })
        .collect();
    if !report.not_worth_doing.is_empty() {
        lines.push(format!(
            "left open for you to decide (not worth doing): {}",
            report
                .not_worth_doing
                .iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines.extend(notes);
    lines.join("\n")
}

fn describe_run(report: &mestari::RunReport) -> String {
    match report {
        mestari::RunReport::Idle => "nothing queued".to_string(),
        mestari::RunReport::Landed { number, pr, rounds } => {
            format!("#{number} landed in {rounds} round(s): {pr}")
        }
        mestari::RunReport::Escalated {
            number, question, ..
        } => format!(
            "#{number} escalated:\n{question}\n  answer: paja work answer {number} \"<your answer>\""
        ),
        mestari::RunReport::Arbitrated { number, answer, .. } => format!(
            "#{number} answered by the arbiter and queued again: {answer}\n  override: paja work \
             answer {number} \"<your answer>\""
        ),
        mestari::RunReport::Stopped { number, .. } => {
            format!("stopped by Ctrl-C; #{number} resumes on the next `paja run`")
        }
        mestari::RunReport::UsageLimit {
            number, resets_at, ..
        } => match resets_at {
            Some(ms) => format!(
                "usage limit; #{number} resumes on the next `paja run` after {}",
                mestari::format_utc(*ms)
            ),
            None => format!("usage limit; #{number} resumes on the next `paja run`"),
        },
        mestari::RunReport::Paused { resets_at } => format!(
            "usage limit not reset; `paja run` can run again at {}",
            mestari::format_utc(*resets_at)
        ),
    }
}

fn describe_recommendations(found: &paja::init::Recommendations) -> String {
    let mut text = String::from("Recommended stacks, best first:\n");
    for (i, o) in found.recommendations.iter().enumerate() {
        text.push_str(&format!(
            "\n{}. {} ({} skeleton)\n   Why: {}\n   Trade-offs: {}\n",
            i + 1,
            o.stack,
            o.skeleton,
            o.why,
            o.tradeoffs
        ));
    }
    if !found.questions.is_empty() {
        text.push_str("\nQuestions that could change this:\n");
        for q in &found.questions {
            text.push_str(&format!("  {q}\n"));
        }
    }
    text.push_str("\nThen: paja init --new NAME --stack <rust|node|python>");
    text
}

fn describe_new(report: &paja::init::NewReport) -> String {
    let mut text = format!(
        "created {} ({}): {}\ncloned to {}\nstack: {} ({} skeleton)\nchecks: {}\nwrote: {}\npushed the first commit to {}\n\nNext:\n",
        report.repo,
        if report.private { "private" } else { "public" },
        report.url,
        report.directory,
        report.stack,
        report.skeleton,
        report.checks.join("; "),
        report.files.join(", "),
        report.default_branch
    );
    for step in &report.next_steps {
        text.push_str(&format!("  {step}\n"));
    }
    text.trim_end().to_string()
}

fn describe_init(report: &paja::init::Report) -> String {
    let found = if report.detected.stacks.is_empty() {
        "no stack Paja knows".to_string()
    } else {
        report
            .detected
            .environment
            .stacks
            .iter()
            .map(|s| {
                if s.dir == "." {
                    s.name.clone()
                } else {
                    format!("{} ({})", s.name, s.dir)
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "detected: {found}\nchecks: {}\nwrote: {}\nci: {}\n",
        if report.detected.checks.is_empty() {
            "none".to_string()
        } else {
            report
                .detected
                .checks
                .iter()
                .map(|c| c.command.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        },
        report.files.join(", "),
        report.ci
    );
    for item in &report.detected.dropped {
        text.push_str(&format!("dropped: {}: {}\n", item.item, item.reason));
    }
    if report.ci == "none" {
        text.push_str(
            "  the repository has no CI; `.paja/ci` says `none` (use --ci to add a workflow)\n",
        );
    }
    if let Some(note) = &report.note {
        text.push_str(&format!("note: {note}\n"));
    }
    text.push_str(&format!(
        "pull request: {}\nPaja starts working once it is merged into {}.",
        report.pull_request, report.default_branch
    ));
    text
}

/// An active item's run in one line: `running: round 2 of 3, fix (opus) since 14:02:11 UTC`.
fn describe_live(run: &serde_json::Value) -> Option<String> {
    let state = run["state"].as_str()?;
    let mut line = format!(
        "{state}: round {} of {}",
        run["round"].as_u64().unwrap_or(0),
        run["max_rounds"].as_u64().unwrap_or(0)
    );
    if let Some(step) = run["step"].as_object() {
        line.push_str(&format!(", {}", step["role"].as_str().unwrap_or("a step")));
        if let Some(model) = step.get("model").and_then(|m| m.as_str()) {
            line.push_str(&format!(" ({model})"));
        }
        if let Some(ms) = step.get("started_ms").and_then(|m| m.as_u64()) {
            let secs = ms / 1000 % 86_400;
            line.push_str(&format!(
                " since {:02}:{:02}:{:02} UTC",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ));
        }
    }
    if let Some(pr) = run["pr"].as_str() {
        line.push_str(&format!(", {pr}"));
    }
    Some(line)
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
    if let (Some(claim), true) = (&item.claim, item.status != jono::Status::Landed) {
        line.push_str(&format!("\n  claimed by runner {}", claim.runner));
        if let Some(until) = &claim.stale_after {
            line.push_str(&format!(", stale after {until}"));
        }
    }
    if let Some(last) = item.answers.last().filter(|a| a.by_arbiter()) {
        line.push_str(&format!("\n  answered by arbiter: {}", last.answer));
        if item.status == jono::Status::Queued {
            line.push_str(&format!(
                "\n  override: paja work answer {} \"<your answer>\"",
                item.number
            ));
        }
    }
    if item.status == jono::Status::Escalated {
        line.push_str(&format!(
            "\n  answer: paja work answer {} \"<your answer>\"",
            item.number
        ));
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
