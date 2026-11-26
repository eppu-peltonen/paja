---
name: status
description: Summarise Paja's queue (active, queued and escalated items), its latest events and any question waiting for an answer. Use when the person asks what Paja is doing, what's queued, or whether Paja needs them.
---

# Paja status

Use only the `paja` MCP tools and don't run shell commands. Apart from the sync in step 1, which imports new issues, change nothing.

1. Call `work_sync` to import the latest issues labelled `paja`. If it fails (for example `gh` isn't signed in), say so in one line and go on with the queue as it is.
2. Call `work_list`. Group the open items by status: **active**, **queued** (oldest first, which is the order Paja takes them), and **escalated**. Leave out landed and closed items, but give their count.
3. For each escalated item, show its number, title, link and its `note`, which is the question Paja asked. End that part with: answer with `/paja:answer`.
4. Call `log` with `limit` 10 and describe the latest events in plain words (for example "round 2 of #31: the checks failed", "landed #12"), newest first.
5. Where an item has a `usage` total, give its estimated cost and turns in one short line.

Keep it short: a few lines per group, no raw JSON.
