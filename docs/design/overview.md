# Overview

Paja is a software factory: it takes work from GitHub issues, runs Claude Code agents on it in isolated
environments, verifies the result and lands it. It builds new projects from their first commit and takes over existing
ones equally well, and it is built by itself.

Its heart is a headless core that holds environments, work, specs and orchestration. Everything else connects to it
through one interface: Claude Code (desktop or CLI) for working hands-on, and Paja Desk for watching and steering the
fleet. Claude Code is also the worker that writes the code, so Paja never reimplements an agent.

## Principles

- **Runs natively on Windows.** Windows is a first-class platform, as are Linux and macOS: Paja builds, tests and
  runs on each without WSL or a VM, and CI runs on Windows and Linux. Platform differences (home directory, private
  files, the shell for checks, hook quoting) live in one module, `platform.rs`; the rest of the core is the same
  everywhere. Until the deny-list hook understands Windows paths, `paja run` and `paja hook` refuse on Windows rather
  than run agents unguarded; issue #23 tracks that port. Shell commands use the POSIX shell that Git for Windows brings, which Claude Code on Windows needs too.
- **One core, many front ends.** The core has no UI of its own. A CLI with JSON output, an MCP server and an HTTP API
  are three doors to the same functions, so Claude Code, scripts and Paja Desk all see the same state.
- **Claude Code does the coding; the core does the plumbing.** Workers are Claude Code runs started headless
  (`claude -p` as a subprocess of the Rust core) inside an environment the core prepared. The core decides what runs
  where; Claude Code decides how to write it.
- **Humans decide what, agents decide how.** People set intent, approve specs and their acceptance criteria, and
  answer escalations. Nobody reviews pull requests: agents and checks decide whether code lands. See
  [landing.md](./landing.md).
- **Verification is the product.** A change counts only when it boots and passes end-to-end checks, before and after
  it reaches `main`. A separate, skeptical evaluator grades the running app against the spec, and some scenarios stay
  hidden from the builder so tests can't be gamed. Checks stay out of reach of the change they judge: Vasara loads its
  rules and scenarios from `main`, never from the branch, and a fix can't edit the test that proves it.
- **Every failure teaches the harness.** Each revert, escalation or human fix becomes a proposed harness rule or
  check, adopted when the eval suite shows it helps. The harness is tested like code: a suite of real chunks reruns
  whenever a prompt, skill, hook or the model changes, and a change that lowers the pass rate is rejected. On each
  model upgrade, harness pieces that no longer help are pruned.
- **Measure outcomes, not activity.** Every run is recorded in the append-only event log. From it Paja counts human
  minutes, first-pass acceptance, fix loops, reverts, cost and lead time per chunk.
- **Two modes, one door.** Pairing (a person and Claude Code, hands-on) is for exploration and unclear work; the queue
  is for work that can be specified. A pairing session turns its findings into a spec in one step.
- **Small batches.** Each chunk has a diff-size budget; an oversized chunk is split, not built.
- **Plain code for plumbing.** Scheduling, routing and gating are deterministic code; LLMs do the judgment work inside
  steps.
- **New or existing projects alike.** Factory files live outside the app repo, so a new repo gets them from its first
  commit and an existing one doesn't have to change.
