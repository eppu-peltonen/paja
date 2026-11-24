# Paja

Paja is a software factory that builds itself. This file is the harness (Kaava) for agents working on Paja's own code.
The design is in `docs/` (start at `docs/index.md`); `docs/glossary.md` names the parts (Jono, Kaava, Posti, Mestari,
Ahjo, Vasara, Paja Desk). Use those names in code, docs and commit messages. When a change alters the design, update
the matching file in `docs/design/` in the same pull request.

## Commands

Run all three before saying a change is done; CI runs the same ones.

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Try the CLI against a throwaway home so you never touch real state:

```sh
PAJA_HOME=$(mktemp -d) cargo run -q -- --json env create demo
```

## Layout

- `crates/paja/src/events.rs`: the append-only event log, the core's only source of truth.
- `crates/paja/src/home.rs`: the state directory (`$PAJA_HOME`, default `~/.paja`) and its lock.
- `crates/paja/src/ahjo.rs`: environments, i.e. git worktrees with their own port blocks.
- `crates/paja/src/mcp.rs`: `paja mcp`, the same core functions as MCP tools over stdio.
- `crates/paja/src/main.rs`: the `paja` CLI. Library code lives in the modules, not here.
- `crates/paja/tests/smoke.rs`: end-to-end test that drives the real binary.
- `evals/`: harness evals that run headless Claude Code (need a signed-in `claude`; not run in CI).
- `docs/`: the design and operator docs, routed from `docs/index.md`.

## Rules

- **State lives only in the event log.** Never add a second store that can disagree with it. Derive current state by
  replaying events. Never rewrite or delete events; change the envelope only with a migration that keeps old logs
  readable.
- **Every write holds the home lock** (`Home::lock`), so sequence numbers and port blocks are never handed out twice.
- **Every command supports `--json`** and prints exactly one JSON value on stdout, errors as `{"error": "..."}`.
  Agents and Paja Desk depend on this. The one exception is `paja mcp`, whose stdout is the MCP protocol; each of its
  tools returns the same JSON as the matching command.
- **The CLI and the MCP server are two doors to one core.** New behaviour goes in the library modules and is exposed
  through both, with the same JSON shape.
- **Errors say what to do.** Include the path, name or command involved and the fix when there is one.
- **Test behaviour end to end.** New CLI behaviour gets a case in `tests/smoke.rs` that runs the real binary.
- **Checks stay out of reach of the change they judge.** A change that fixes a failing test must not edit that test;
  Vasara's rules and scenarios are read from `main`, never from the branch under review.
- **Small changes.** One concern per pull request.
- **This repository is public.** Never commit secrets, private links, personal details or internal hostnames.
- **Docs describe Paja, nothing else.** `docs/` and other Markdown cover only how the system works and what agents
  need to work in it. Leave out references to other projects, research, and notes on inspiration or lineage.

## How changes land

No human reviews pull requests. The person sets intent, approves specs and answers escalations; agents review and land
code.

- **Every change** needs green CI and an independent reviewer agent that did not write it. The reviewer reads the
  diff cold, against the spec or issue, and decides whether it lands.
- **High-risk changes** pass a stricter gate instead of waiting for a person: two independent reviewer agents who
  must agree, plus the full eval suite once it exists. High risk means anything touching sandbox and secrets
  handling, the deny-list hook, landing and revert logic, Vasara's rules, the harness evals, or the event log's
  format (`events.rs`). Automation decides the tier from the files a change touches.
- **Escalate, don't wait.** If reviewers disagree, a check can't decide, or a change would weaken a check, the
  question goes to Posti (until Posti exists: a comment on the issue or pull request) as an intent decision, with
  options and a recommendation. Details: `docs/design/landing.md`.
- **Until Paja's own reviewer runs (build step 3)**, Claude does the review and landing for these hand-built seed
  pull requests under the same rules.
