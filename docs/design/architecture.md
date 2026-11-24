# Architecture

One headless core, Paja, with Claude Code and Paja Desk as two doors into it. The front ends never talk to each other,
only to the core, so either can be added, replaced or left out. Claude Code appears twice on purpose: once as a front
end a person types into, and once as the worker that Mestari starts on its own.

```mermaid
flowchart TB
    cc["Claude Code (desktop or CLI)<br/>plugin: skills, deny-list hook, MCP config"]
    desk["Paja Desk<br/>board, inbox, metrics, history"]
    iface["Interface: CLI with --json · MCP server · HTTP API"]
    subgraph core["Paja core"]
        jono["Jono<br/>work queue"]
        kaava["Kaava<br/>specs and harness"]
        posti["Posti<br/>inbox"]
        mestari["Mestari<br/>orchestrator"]
        ahjo["Ahjo<br/>environments"]
        vasara["Vasara<br/>verifier"]
        log[("event log")]
    end
    workers["Workers: headless Claude Code runs<br/>one per step, inside an Ahjo environment"]
    repos["App repos, new or existing<br/>no factory files inside"]

    cc -- "MCP, CLI" --> iface
    desk -- "HTTP API" --> iface
    iface --> core
    mestari -- starts --> workers
    workers -- "land verified changes" --> repos
```

## Units

| Unit | Job | First version built with |
| --- | --- | --- |
| **Ahjo** (environments) | An isolated environment per chunk: git worktree, ports, database, services, seed data. Each one sandboxed and failing closed (no run if the sandbox can't start, hooks block when they fail), with a GitHub token that reaches only the repository being worked on | git worktrees, Docker Compose |
| **Jono** (queue and scheduler) | Work items with priority and dependencies; lease each chunk to one worker; pick the next one with plain rules; enforce a diff-size budget per chunk; pace work to the account's usage limits | GitHub issues labelled `paja`, event log |
| **Kaava** (specs and harness) | Versioned specs with acceptance criteria and a risk tier; held-out scenarios the builder never sees; an eval suite of real chunks that tests the harness itself; the domain harness and canon rules, copied into each environment as `CLAUDE.md` and skills | Markdown in the repo |
| **Mestari** (orchestrator) | Run workflow graphs: spec, plan, build, verify, review, land, post-land verify. Fix loops are capped at three rounds, after which the builder rethinks the approach instead of patching. Planner, builder, reviewer and evaluator are separate roles. A learn step after landing drafts harness rules from failures | YAML graphs; `claude -p` subprocesses |
| **Vasara** (verifier) | Load rules and scenarios from `main`, never from the branch; boot the environment and run deterministic and end-to-end checks; an independent, skeptical evaluator agent grades the running app against the acceptance criteria and held-out scenarios; after landing, re-run on `main` and revert on failure | The app's own test commands, Playwright |
| **Posti** (inbox) | Collect every agent question and escalation; send one digest, each item with options and a recommendation; mirror status to the issue | Issue comments, event log |
| Interface | One set of core functions exposed three ways, all reading from the event log | CLI (`paja ... --json`), MCP server (official Rust MCP SDK), HTTP API |
| Claude Code plugin | Skills such as `/paja-spec`, `/paja-status` and `/paja-answer`, a skill that turns a pairing session into a spec, a deny-list hook, and the MCP server config | Claude Code plugin |
| **Paja Desk** | Board of chunks, Posti inbox, outcome metrics, spend and history, usable from a phone | Web app over the HTTP API, TypeScript types generated from the core |

## State

The event log is the only store. It is append-only JSON lines in `$PAJA_HOME/events.jsonl`; every part derives its
current state by replaying it, and every write holds the home lock. See `crates/paja/src/events.rs`.

A write cut off by a crash leaves bytes after the last newline. Those were never acknowledged, so the next append
cuts them off and keeps them in `events.jsonl.torn`; a bad line that ends in a newline is an error, never repaired.

## Work queue

Jono's items are GitHub issues labelled `paja`. `paja work sync` reads the open ones with `gh` and appends
`work.imported`, `work.changed`, `work.closed` and `work.reopened` events. Whether an issue is open is kept apart from
the item's status (`queued`, `active`, `landed`, `escalated`, set with `work.status`), so an escalation's question
survives the issue closing and reopening; a landed issue that reopens is queued again. `paja work claim` takes the oldest
open, queued item and marks it active under the home lock, so two runs never share one. A sync that reads its full limit
of issues refuses to close anything rather than drop work it could not see.

## The run loop

`paja run` is Mestari's first graph. It fetches `origin/main`, syncs Jono, claims the next item and creates an Ahjo
environment `work-<issue>` from `main` (resuming the branch an earlier run kept). Then, for at most three rounds:

1. A headless `claude -p` builder implements the issue and commits. It may use only file tools, `cargo` and local git.
2. Vasara runs each command in `.paja/checks`, read from `main`. A failure goes back to the builder.
3. A fresh reviewer agent with read-only tools reads `git diff main...HEAD` cold against the issue and `CLAUDE.md` as it
   is on `main`, and answers `land`, `changes` or `escalate`. A change touching a prefix in `.paja/high-risk` (read
   from `main`) gets two reviewers, who must agree; a split is escalated.

On `land`, Mestari pushes the branch, opens a pull request, waits for CI with `gh pr checks --watch`, squash-merges it
and marks the item landed. On `escalate`, three failed rounds, a red CI or an error it can't fix, it comments on the
issue with the question and marks the item escalated; the branch keeps the work. Agents run with a throwaway
`PAJA_HOME`, so they never touch the factory's own state. Each step is recorded as `run.started`, `run.round`,
`run.landed` or `run.escalated`. `paja run --every 5` keeps going, checking an empty queue every five minutes.

## The deny-list hook

Every agent Mestari starts runs with `--setting-sources ""` and settings that send each tool call to
`paja hook --root <environment> --home <factory home>` as a Claude Code `PreToolUse` hook. No settings file is loaded,
so neither the owner's settings nor a file on the branch can loosen the hook or switch it off; `CLAUDE.md` still loads.
The bounds are passed in rather than read from the tool call, because the reported `cwd` moves when the agent runs `cd`.

- **Shell commands** are split into simple commands at `;`, `&&`, `||`, `|`, `&`, subshells and substitutions (also
  inside double quotes), with quotes and backslashes removed. Each must start with a program on an allowlist, named
  without a path: cargo, local git, and plain file and text tools. Shells, interpreters, `env`, `xargs` and network
  tools are not on it. Git is limited to local subcommands (add, commit, status, diff, log, show, stash and the like);
  cargo can't publish or install; `find` can't run or delete; `sed -i` is out. Setting `PATH`, `GIT_*`, `LD_*` or
  `RUSTC_WRAPPER` is blocked.
- **Writes** stay in the environment: file edits, `rm`, `cp`, `mv`, `mkdir`, `touch`, `cd` and output redirections
  must resolve inside the root, and a path the shell would expand first (`~`, `$`, globs) is blocked.
- **Reads** may look outside the worktree, but nothing may touch credential paths (`~/.ssh`, `~/.config/gh` and the
  like) or the factory home outside the environment, including via `Glob` patterns and `Grep` globs.
- **Network tools** (`WebFetch`, `WebSearch`) are blocked.

It fails closed: unreadable input, missing bounds and a crash all block. Before each agent Mestari checks that the
hook blocks a `git push` with the deny-list's message and allows `cargo test`, and refuses to start the agent if
not. The hook is a guardrail, not a sandbox: code the agent writes still runs under `cargo test`.

## MCP server

`paja mcp` serves the core as MCP tools over stdio (`env_*`, `work_*` and `log`), built on
the official Rust MCP SDK. Claude Code starts it from the project directory, so a tool's `repo` defaults to there. Each
tool returns the JSON the matching `paja --json` command prints, and a failure is a tool error whose text is
`{"error": "..."}`.

## CLI output

Under `--json` every command prints exactly one JSON value on stdout. Failures, usage errors included, carry an
`error` key and exit non-zero; `--help` prints `{"help": ...}` and `--version` prints `{"version": ...}`.
