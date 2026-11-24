# Paja

Paja is a software factory that builds itself. This file is the harness (Kaava) for agents working on Paja's own code.
Design: https://claude.ai/artifact/MATj1wWDEtX7Lqt77yrXd6. The README names the parts (Jono, Kaava, Posti, Mestari,
Ahjo, Vasara, Paja Desk); use those names in code, docs and commit messages.

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
- `crates/paja/src/main.rs`: the `paja` CLI. Library code lives in the modules, not here.
- `crates/paja/tests/smoke.rs`: end-to-end test that drives the real binary.

## Rules

- **State lives only in the event log.** Never add a second store that can disagree with it. Derive current state by
  replaying events. Never rewrite or delete events; change the envelope only with a migration that keeps old logs
  readable.
- **Every write holds the home lock** (`Home::lock`), so sequence numbers and port blocks are never handed out twice.
- **Every command supports `--json`** and prints exactly one JSON value on stdout, errors as `{"error": "..."}`.
  Agents and Paja Desk depend on this.
- **Errors say what to do.** Include the path, name or command involved and the fix when there is one.
- **Test behaviour end to end.** New CLI behaviour gets a case in `tests/smoke.rs` that runs the real binary.
- **Checks stay out of reach of the change they judge.** A change that fixes a failing test must not edit that test;
  Vasara's rules and scenarios are read from `main`, never from the branch under review.
- **Small changes.** One concern per pull request.

## Risk tiers

These areas always wait for a human before landing, even when checks pass: sandbox and secrets handling, the deny-list
hook, landing and revert logic, Vasara's rules and the harness evals, and the event log's format (`events.rs`).
Everything else may land on green.
