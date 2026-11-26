# Decisions

Settled decisions, newest first within each date. Change one only by adding a new entry that says what it replaces.

## 2026-11-26

- **Paja is licensed under Apache-2.0.** The full text is in `LICENSE` at the repository root, and the crate's
  `license` field says the same.

## 2026-11-24

- **Self-hosting first.** Steps 2 and 3 are cut to the smallest loop that can build Paja (issue import, build, review,
  land, deny-list hook); everything else is built by that loop. This replaces the earlier step 2 (interface and plugin
  by hand).
- **No separate GitHub identity.** Commits and pull requests go out under the owner's account. Unattended runs use a
  fine-grained token limited to the repository being worked on, so an agent can't reach anything else.
- **Nobody reviews pull requests.** Agents review and land code; high-risk changes pass a stricter automated gate
  instead of a person. See [landing.md](./landing.md).
- **Paja's first target is Paja itself.** The factory builds the factory; other projects come in step 7.
- **The core is written in Rust.** Workers are `claude -p` subprocesses with streamed JSON output, so no Agent SDK is
  needed. The MCP server uses the official Rust MCP SDK, and Paja Desk gets TypeScript types generated from the core's
  API.
- **Work comes from GitHub issues.** Jono imports issues labelled `paja`; `/paja-spec` creates them. The approved spec
  version lives in Kaava and is linked from the issue, and status and Posti questions are mirrored as issue comments.
- **Workers run on the owner's machine only** for now. Ahjo uses git worktrees with their own ports; no cloud VMs yet.
- **The budget is the account's Claude usage limits, not money.** Jono paces work so a chunk doesn't start when too
  little of the current limit window is left. Dollar caps matter only if Paja later runs on an API key.
- **One repository.** The core, the Claude Code plugin and Paja Desk live here, each in its own directory.

## Risks

- Headless Claude Code runs need their permissions and tools set explicitly, so a too-loose allowlist is the main
  safety hole.
- Vague specs produce confident wrong code, so the spec gate is only as good as the review behind it.
- A weak end-to-end suite makes post-land verification meaningless, which is why step 1's exit test matters most.
