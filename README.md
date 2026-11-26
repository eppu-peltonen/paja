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

Paja runs natively on Windows, Linux and macOS. [`docs/setup.md`](docs/setup.md) lists what to install and how to
start Paja on its own queue (`paja run --every 5`). Every command also takes `--json`:

```sh
paja env create fix-login        # isolated worktree on branch paja/fix-login with its own ports
paja --json env show fix-login
paja work list                   # the queue, imported from issues with the work label
paja log                         # the append-only event log
```

To work with Paja from Claude Code (`/paja:init`, `/paja:doctor`, `/paja:status`, `/paja:answer`, `/paja:spec` and the MCP tools), install the plugin:

```sh
claude plugin install paja --marketplace eppu-peltonen/paja
```

State lives in `$PAJA_HOME` (default `.paja` in your home directory). See [CLAUDE.md](CLAUDE.md) for how to work on
Paja.

## License

Paja is licensed under the [Apache License 2.0](LICENSE).
