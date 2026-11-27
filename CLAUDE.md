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

In PowerShell on Windows: `$env:PAJA_HOME = (New-Item -ItemType Directory (Join-Path $env:TEMP (New-Guid))).FullName;
cargo run -q -- --json env create demo`.

Agents Mestari starts already have a throwaway `PAJA_HOME`, and Paja's deny-list hook doesn't let them set it. They
try read-only commands such as `cargo run -q -- --json work list`, and leave `env create`, which adds a worktree to
the real repository, to the smoke tests.

## Layout

- `crates/paja/src/events.rs`: the append-only event log, the core's only source of truth.
- `crates/paja/src/home.rs`: the state directory (`$PAJA_HOME`, default `~/.paja`) and its lock.
- `crates/paja/src/ahjo.rs`: environments, i.e. git worktrees with their own port blocks.
- `crates/paja/src/jono.rs`: the work queue, imported from GitHub issues with the work label (`paja` here; `gh`,
  or `$PAJA_GH`).
- `crates/paja/src/mestari/`: `paja run`, the build, check, review and land loop for one work item. Only the
  high-risk parts are listed in `.paja/high-risk`: `mod.rs` (the round loop, main's configuration, the risk tier,
  the review prompt and verdicts), `agent.rs` (`claude()`, the hook settings and self-test, the sandbox glue,
  `GitGuard`), `arbiter.rs` (the arbiter agent that settles an escalation before it reaches the person, and the
  rules that overrule it), `land.rs` (landing, waiting for CI, escalation), `triage.rs` (`paja triage`: an agent
  closes duplicate and already-fixed follow-ups) and `detect.rs` (the `init` agents' sandboxes and tool lists).
  `usage.rs` (what a step spent), `progress.rs` (stderr text), `follow_ups.rs`, `builder.rs` (the builder's question
  and answers), `pool.rs` (`paja run --workers`: lanes, each a `paja run --item` of its own), `plan.rs` (`paja pool
  plan`: the files an item will touch, which the pool admits it by) and `live.rs` (where a run is, read from the log)
  are normal risk. Outside `mestari/`, `init/generate.rs` (the programs a generated skeleton's agent may run) is high
  risk too.
- `.paja/checks`, `.paja/high-risk`, `.paja/models`, `.paja/commands`, `.paja/labels`: Vasara's check commands, the
  high-risk path prefixes, the model per agent role, the programs agents may run besides local git (`review <program>`
  for reviewers too; `cargo` when missing) and the repository's `work` and `follow-up` issue labels (`factory` and
  `factory-follow-up` when missing). `paja run` reads them from `main`, never from the branch under review.
- `crates/paja/src/hook.rs`: the deny-list hook (`paja hook`) every agent Mestari starts runs under. It allows local
  git and the programs Mestari passes from `.paja/commands`, and it fails closed.
- `crates/paja/src/posti.rs`: Posti, the notices that tell the person a run needs them (desktop, `PAJA_NOTIFIER`,
  a push URL), each recorded as `notify.sent` or `notify.failed`.
- `crates/paja/src/mcp.rs`: `paja mcp`, the same core functions as MCP tools over stdio.
- `crates/paja/src/platform.rs`: what differs between Windows and Unix. Paja runs natively on both; keep
  `std::os::unix`, `/`-only paths and `sh` assumptions out of the other modules' non-test code.
- `crates/paja/src/main.rs`: the `paja` CLI. Library code lives in the modules, not here.
- `desk/`: Paja Desk, a Tauri 2 app (`paja-desk`) that calls the core in-process; `desk/ui/` is plain HTML, CSS and
  JavaScript. Not a default workspace member: build and test it with `-p paja-desk` (on Linux it needs WebKitGTK's
  development packages). Its Tauri permissions (`desk/capabilities/`, `desk/tauri.conf.json`) and its installer (`desk/tauri.bundle.json`,
  `desk/installer.nsh`, which edits PATH) are high risk.
- `crates/paja/tests/smoke.rs`: end-to-end test that drives the real binary. Its fake `gh` and `claude` are shell
  scripts; on Windows `crates/paja/examples/fake_tool.rs` runs them, so run the whole `cargo test`, which builds it.
- `evals/`: harness evals that run headless Claude Code (need a signed-in `claude`; not run in CI).
- `docs/`: the design and operator docs, routed from `docs/index.md`.

## Rules

- **State lives only in the event log.** Never add a second store that can disagree with it. Derive current state by
  replaying events. Never rewrite or delete events; change the envelope only with a migration that keeps old logs
  readable.
- **Every write holds the home lock** (`Home::lock`), so sequence numbers and port blocks are never handed out twice.
- **Every command supports `--json`** and prints exactly one JSON value on stdout, errors as `{"error": "..."}`.
  Agents and Paja Desk depend on this. Three exceptions: `paja mcp`, whose stdout is the MCP protocol (each tool
  returns the same JSON as the matching command); `paja run --every`, which prints one JSON value per line, one per
  item, for as long as it runs; and `paja hook`, which Claude Code reads by exit code and stderr only (it is Claude
  Code's hook, so it has no MCP tool). `paja run`, with or without `--every`, also writes progress lines to stderr.
- **The CLI and the MCP server are two doors to one core.** New behaviour goes in the library modules and is exposed
  through both, with the same JSON shape.
- **Errors say what to do.** Include the path, name or command involved and the fix when there is one.
- **Test behaviour end to end.** New CLI behaviour gets a case in `tests/smoke.rs` that runs the real binary.
- **Checks stay out of reach of the change they judge.** A change that fixes a failing test must not edit that test;
  Vasara's rules and scenarios are read from `main`, never from the branch under review.
- **Small changes.** One concern per pull request.
- **This repository is public.** Never commit secrets, private links, personal details or internal hostnames. A
  test that needs a token-shaped string (`ghp_…`, `Bearer …`, a JWT) builds it at run time from parts, never writes it
  as one literal, so secret scanners have nothing to flag.
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
- **Review converges.** At most five review rounds per change. Every finding in the change (bug, risk, missing test,
  nit) is fixed in that change: the reviewer's findings go back to the builder as the next round's feedback, and
  the change lands when a round leaves nothing to fix. No follow-up issue is filed. At round five an open blocker (a
  bug in what the issue asks for, a security hole the change opens, a broken rule, missing tests) escalates; any
  other open finding, and a real bug in code the change doesn't touch, is listed in the pull request and the change
  lands. A re-review reads the fix and checks the earlier findings are settled (high risk: the whole change). Each
  review runs in its own fresh session. Details: `docs/design/landing.md`.
- **Until Paja's own reviewer runs (build step 3)**, Claude does the review and landing for these hand-built seed
  pull requests under the same rules.
