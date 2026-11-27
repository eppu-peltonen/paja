# Paja

Paja (Finnish: *workshop, forge*) is a software factory: a headless core that takes work from GitHub issues, runs
Claude Code agents on it in isolated environments, verifies the result, and lands it. It works for new projects and
existing ones alike, and it is built by itself.

The design lives in [`docs/`](docs/index.md): start with the [overview](docs/design/overview.md) and the
[architecture](docs/design/architecture.md).

## Parts

| Name | Meaning | What it is |
| --- | --- | --- |
| Paja | workshop, forge | The whole factory and its headless core |
| Jono | queue | Work queue and scheduler, fed by GitHub issues with the repository's work label (`paja` here) |
| Kaava | pattern, formula | Specs, scenarios, harness evals and the harness agents work from |
| Posti | mail | Inbox for agent questions and escalations |
| Mestari | master craftsman | Orchestrator that runs workflow graphs |
| Ahjo | forge hearth | Isolated environments where the work runs |
| Vasara | hammer | Verifier |
| Paja Desk | workshop desk | App for watching and steering the fleet |

## Getting started

To use Paja on your own project, start with the [quickstart](docs/quickstart.md): install, set up the repository,
write the first issue and watch Paja land it.

Paja runs natively on Windows, Linux and macOS. Each [GitHub release](https://github.com/eppu-peltonen/paja/releases)
has a prebuilt `paja` per platform with a `SHA256SUMS` file, plus Paja Desk's installers; the steps are in
[Install a release](docs/setup.md#install-a-release), or [build from source](docs/setup.md#build-from-source) with
Rust. [`docs/setup.md`](docs/setup.md) also covers setting up a repository and starting Paja on its queue
(`paja run --every 5`). Every command also takes `--json`:

```sh
paja init                        # add Paja's files (.paja/, CLAUDE.md) to a project
paja doctor                      # check this machine and the repository, with the fix for each problem
paja work list                   # the queue, imported from issues with the work label
paja run --every 5               # build, check, review and land queued items; when idle, look again every 5 min
paja usage                       # what the runs spent against your Claude plan
paja env create fix-login        # isolated worktree on branch paja/fix-login with its own ports
paja log                         # the append-only event log
```

[Paja Desk](docs/setup.md#paja-desk) is the desktop app: it checks a repository, starts and stops runs, and follows
the running item live. [`docs/usage.md`](docs/usage.md) shows what an item usually costs and how `.paja/models`
changes it; [`docs/safety.md`](docs/safety.md) covers what agents can and can't do on your machine.

To work with Paja from Claude Code (`/paja:init`, `/paja:doctor`, `/paja:status`, `/paja:answer`, `/paja:spec` and the MCP tools), install the plugin:

```sh
claude plugin install paja --marketplace eppu-peltonen/paja
```

State lives in `$PAJA_HOME` (default `.paja` in your home directory). See [CLAUDE.md](CLAUDE.md) for how to work on
Paja.

## License

Paja is licensed under the [Apache License 2.0](LICENSE).
