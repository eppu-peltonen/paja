# Usage: how much of a Claude plan Paja uses

Every agent step Mestari starts runs on your Claude plan and counts against its usage limits. This page says what one
work item usually takes, what drives the cost, and how to watch it.

## What one item usually takes

Measured on 2026-11-27 with `paja usage` on Paja's own event log (a Rust project), over 44 landed items:

| Per landed item | Median | 90th percentile |
| --- | --- | --- |
| Review rounds | 1 | 2 |
| Tokens (input, output, cache read and cache write) | 781,385 | 3,523,605 |
| Wall time | 4 min 34 s | not measured yet |

These are Paja's own numbers. Other projects, with other sizes and slower checks, will differ; run `paja usage` after a
few items to get yours. Most of the tokens are cache reads, which a session pays each turn it re-reads its context.

## What drives the cost

- **Roles.** A normal item is a build step, a first review and, for high-risk changes, a second review. A review that
  asks for changes adds a fix step and a re-review. Items that take more rounds cost more.
- **`.paja/models`.** One line per role sets its model and optionally its effort. Models differ most in cost: the
  build and fix roles are the most steps, the first review reads the most. A role with no line runs on your account's
  default model.
- **`max-turns`.** Caps the turns of one session of a role (80 for build and fix, 30 for reviews when no line says
  otherwise). Fewer turns cap the worst case, at the price of an item sometimes needing another round.
- **Lanes.** `paja run --workers N` runs N items at once, so a window is used up N times as fast.

Paja's own `.paja/models` is the default to compare with: build and fix on sonnet, the first review on opus, the
re-review and the second review on sonnet with medium effort. For a smaller plan, copy
[`usage-models.example`](./usage-models.example) to `.paja/models` on your default branch. It moves fix, the
re-reviews and the cheap roles to haiku, reviews on sonnet, lowers the turn caps and starts no build once 80% of the
window is used. Expect cheaper items that sometimes need more rounds; if it does, raise the caps.

## At a usage limit

When a step stops because the plan's usage limit is reached, the run stops, keeps the work and resumes after the
reset. When the recorded usage window is nearly used up (90% by default, `window <percent>` in `.paja/models`),
Mestari starts no build or claim until the reset; reviews still start.

## Watching spend

```sh
paja usage                    # per item and per day, with the totals and the figures above
paja usage --since 2026-11-01 # only steps from that date (UTC)
paja usage --item 177         # one item
paja --json usage             # the same as JSON
```

Per item you get the rounds, the steps by role and model, tokens, turns, wall time and outcome. Steps whose usage
couldn't be read are counted as unknown, never as zero.
