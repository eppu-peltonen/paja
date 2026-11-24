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
| Jono | queue | Work queue and scheduler, fed by GitHub issues labelled `paja` |
| Kaava | pattern, formula | Specs, scenarios, harness evals and the harness agents work from |
| Posti | mail | Inbox for agent questions and escalations |
| Mestari | master craftsman | Orchestrator that runs workflow graphs |
| Ahjo | forge hearth | Isolated environments where the work runs |
| Vasara | hammer | Verifier |
| Paja Desk | workshop desk | App for watching and steering the fleet |

## Status

Step 1 of the build order: the seed, written by hand. Today Paja has the event log and Ahjo environments:

```sh
cargo install --path crates/paja
paja env create fix-login        # isolated worktree on branch paja/fix-login with its own ports
paja env list
paja --json env show fix-login   # every command speaks JSON
paja env remove fix-login        # the branch is kept
paja log                         # the append-only event log
```

State lives in `$PAJA_HOME` (default `~/.paja`). See [CLAUDE.md](CLAUDE.md) for how to work on Paja.
