# Changelog

Each release's section becomes its release notes. Add what a change means for someone using Paja under Unreleased;
when tagging `vX.Y.Z`, rename Unreleased to `X.Y.Z` (the release workflow reads that section) and start a new one.

## Unreleased

## 0.2.0

- Agents run in Docker Sandboxes by default when `sbx` is installed: Paja finds it on `PATH`, checks it works, and
  stops the step with the fix when it doesn't, never falling back to the host. `PAJA_SBX=off` runs agents on the host.
  `paja doctor`, the first progress line and `run.started` say which backend a run uses and why.
- A sandboxed agent no longer hangs when it exits before reading all of its prompt, and each step's sandbox belongs to
  its own thread.
- Waiting for CI has a limit: 120 minutes by default, or `wait <minutes>` in `.paja/ci`. CI still going at the limit
  escalates the item instead of holding every other landing, and `stop now` ends the wait at once.
- Changes waiting to land take the turn in the order they began to wait, and a run resumed after its review approved
  lands without being reviewed again.
- Landing is sturdier: only a CI check that never started is retried when main moves, a cancelled check or a merge that
  conflicts at landing escalates, a failed merge is explained from GitHub's own answer, and the default branch is
  taken from GitHub before `origin/HEAD`.
- Usage pacing reads the usage window Claude Code really reports, and `paja doctor` warns when no recent step reported
  one. A nearly used window pauses a run before it calls GitHub.
- The arbiter overrules any answer that may weaken a check, and a reviewer saying that no check is weakened no longer
  keeps it out.
- `paja run --workers` plans the next items while every lane is busy, keeps a plan across pool runs until the item's
  title or text changes, and ends its planners' agents however the pool stops.
- A parked item's `work-<n>` environment is freed and its branch carried over when it runs again or its issue closes,
  and `paja doctor` names every environment left over, with the `paja env remove` that frees it.
- Paja Desk opens on the lanes: a card per lane with a + to add one and a - on each, a side panel with what is next,
  held and out, an activity strip and every everyday action at hand. Pool status carries what it needs (each lane's
  round, time on the lane and tokens, and the latest items out with their outcome). Stopping a run from Desk ends its
  agents and checks too, and closing says what it waits for.
- `paja init`'s agent reads the repository's `CLAUDE.md` as data, not instructions, and monorepo check lines run end
  to end.
- `paja --version` built inside some other git repository (a `cargo install` under a home directory that is one) says
  `unknown` instead of naming that repository's commit, and a build without a reflog no longer rebuilds every time.
- Review findings are fixed in the change that drew them (up to five rounds) instead of being filed as follow-up issues;
  what is left at the round limit, or lies outside the change, is listed in the pull request.
- `paja usage` gives the 90th percentile of wall time per landed item in its summary line, next to the median.
- `paja doctor` quotes and escapes the role words it names as skipped in `.paja/models`, so a control character in the
  file never reaches the terminal.
- The events a run writes while it waits for and takes the turn to land are now `run.merging` and `run.landing`, since
  single runs write them as pool lanes do; logs with the old `pool.merging` and `pool.landing` still read the same.

## 0.1.0

The first release: Paja for your own projects, on Windows, macOS and Linux. Start with `docs/quickstart.md`.

- Any language and many repositories. Each repository says how to check a change (`.paja/checks`), which programs
  agents may run (`.paja/commands`) and how to set up an environment (`.paja/setup`), all read from its default branch.
  Work items are kept per repository, and Paja lands through the repository's own default branch and merge method.
- `paja init` adds Paja's files to a project in one pull request (`--new` starts a new one), and `paja doctor` checks
  the machine and the repository, with the fix for each problem.
- Safe defaults: every agent runs in Claude Code's auto mode under Paja's deny-list hook, which fails closed. Docker
  Sandboxes are an optional extra layer (`PAJA_SBX`), and doctor warns when none is set. See `docs/safety.md`.
- Usage pacing: Paja starts no build when the usage window is nearly used up, pauses at a usage limit and picks up
  again after the reset. `paja usage` shows what each item, role and day spent, and `docs/usage.md` gives typical
  numbers and a smaller-plan `.paja/models`.
- Several runners can share one queue: a run claims its item on GitHub (a `<work label>-claimed` label and a claim
  comment, stale after 6 hours), and other runners skip it. `paja run --workers` runs several items at once on one
  machine.
- Review findings that don't block a change are filed together as one follow-up issue, and `paja triage` closes
  duplicate and already-fixed follow-ups.
- Posti tells you when a run needs you: a desktop notification, a program of your own, or a push URL.
- A Claude Code plugin (`/paja:init`, `/paja:doctor`, `/paja:status`, `/paja:answer`, `/paja:spec` and the MCP tools).
- Paja Desk starts and stops runs: add a repository by its checkout folder, check it with `paja doctor`, run one item
  or keep running, and stop after the step or at once. `paja run --stop-on-stdin` is how it stops them.
- Paja Desk follows a running item live: its round, current step and model, how long the step has run, what each
  finished step spent and the pull request. `paja work show` includes the same `run` for an active item, and a run
  that stopped shows as interrupted.
- Prebuilt `paja` binaries for Windows, macOS (Apple silicon and Intel) and Linux, attached to each GitHub release with
  a `SHA256SUMS` file.
- Paja Desk installers for Windows, macOS and Linux in each release, each with the matching `paja`; on Windows it goes
  on your `PATH`. Desk shows the `paja` it found and warns when its version differs.
- `paja --version` names the commit the binary was built from; with `--json` it prints `{"version", "commit"}`.
