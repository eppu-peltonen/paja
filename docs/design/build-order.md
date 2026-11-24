# Build order

Paja builds itself. A small seed is written by hand in pairing sessions with Claude Code; from step 3 on, every change
to Paja goes through Paja's own queue. Workers run on the owner's machine for now. Autonomy widens only after each
step's exit test passes.

1. **Seed, by hand.** The repo, its own harness (`CLAUDE.md`, commands, a smoke test), the event log, and Ahjo's
   `paja env create` giving an isolated git worktree with its own ports.
   - *Exit:* Claude Code, inside a fresh environment, can build Paja and run its tests with one command each.
2. **Interface and plugin, still by hand.** Wrap the core in the CLI and MCP server and ship the Claude Code plugin.
   Every action writes to the event log, and per-chunk metrics are recorded from the start. Before step 3 lets agents
   run unattended, add the deny-list hook that blocks when it errors, and give Paja its own GitHub App identity, so the
   first unattended run is already fail-closed and acts under Paja's name.
   - *Exit:* from a Claude Code session you can create a work item, start an environment and see status without
     leaving the chat.
3. **First loop: Paja starts building Paja.** Jono with its diff-size budget, and a Mestari graph (plan, build, verify
   with a separate evaluator, review) that runs headless Claude Code per step, with an independent reviewer agent
   deciding whether each change lands. Paja's own backlog is the first queue, and each chunk that lands cleanly is
   saved as a harness eval case.
   - *Exit:* 20 of Paja's own chunks landed by the reviewer agent, none reverted and none needing a human fix.
4. **Spec gate, post-land verify, Posti.** Add the spec gate and automated risk tiers. Run the high-risk gate in
   observe mode until its verdicts match a later independent audit, then let high-risk changes land through it too.
   Add revert on a red `main`, the learn step, and the Posti digest of questions and proposed rules.
   - *Exit:* a week of Paja changes landing unattended, every revert explained.
5. **Safety and limits.** Sandbox each environment on the host and fail closed (no run if the sandbox can't start),
   scoped short-lived tokens minted from Paja's GitHub App, and pacing that keeps runs inside the account's usage
   limits.
   - *Exit:* a deliberate prompt-injection test cannot reach a secret or push outside its branch.
6. **Paja Desk, built by Paja.** Write its specs and let the queue build it on the HTTP API.
   - *Exit:* a night's work can be reviewed and steered from a phone.
7. **Other projects.** Point Paja at a second repo: a new one starts from a walking skeleton, an existing one gets its
   services, ports and seed data described once. On each model upgrade, run a pruning pass that removes harness pieces
   no longer earning their cost.
   - *Exit:* a chunk lands in another repo without any Paja-specific change to that repo.

## Status

Step 1 is done. Its exit test is `evals/step1-exit.sh`: headless Claude Code, in a fresh environment and guided only by
`CLAUDE.md`, builds Paja and runs fmt, clippy and the tests. It passed on 2026-11-24. It needs a signed-in `claude`, so
it runs on the owner's machine, not in CI. Step 2 is next.
