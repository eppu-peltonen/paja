# Build order

Paja builds itself. A small seed is written by hand in pairing sessions with Claude Code, only as much as the loop
needs to run; from step 3 on, every change to Paja goes through Paja's own queue. Workers run on the owner's machine
for now. Autonomy widens only after each step's exit test passes.

1. **Seed, by hand.** The repo, its own harness (`CLAUDE.md`, commands, a smoke test), the event log, and Ahjo's
   `paja env create` giving an isolated git worktree with its own ports.
   - *Exit:* Claude Code, inside a fresh environment, can build Paja and run its tests with one command each.
2. **The smallest loop that can build Paja, still by hand.** Only what self-hosting needs, nothing more:
   - `paja mcp`, the core as MCP tools (done).
   - Jono: `paja work sync` imports open GitHub issues labelled `paja` as work items, and `paja work next` picks one.
   - Mestari's first graph, `paja run`: build the item in a fresh Ahjo environment with headless Claude Code, run the
     checks, have an independent reviewer agent read the diff cold, then land it or loop back (at most three rounds),
     and escalate as an issue comment when it can't decide.
   - The deny-list hook, blocking when it errors, so the first unattended run is already fail-closed.
   - *Exit:* an issue labelled `paja` lands on `main` through `paja run`, with no human touching the code.
3. **Paja builds Paja.** From here every change to Paja is an issue run through the loop on the owner's machine; the
   owner writes issues and answers escalations. The backlog starts with what step 2 left out: the Claude Code plugin,
   per-chunk metrics, Jono's diff-size budget, plan and verify steps in the graph, and saving each clean landing as a
   harness eval case.
   - *Exit:* 20 of Paja's own chunks landed by the reviewer agent, none reverted and none needing a human fix.
4. **Spec gate, post-land verify, Posti.** Add the spec gate and automated risk tiers. Run the high-risk gate in
   observe mode until its verdicts match a later independent audit, then let high-risk changes land through it too.
   Add revert on a red `main`, the learn step, and the Posti digest of questions and proposed rules.
   - *Exit:* a week of Paja changes landing unattended, every revert explained.
5. **Safety and limits.** Sandbox each environment on the host and fail closed (no run if the sandbox can't start),
   a fine-grained GitHub token limited to the repository being worked on, and pacing that keeps runs inside the account's usage
   limits.
   - *Exit:* a deliberate prompt-injection test cannot reach a secret or push outside its branch.
6. **Paja Desk, built by Paja.** Write its specs and let the queue build it on the HTTP API.
   - *Exit:* a night's work can be reviewed and steered from a phone.
7. **Other projects.** Point Paja at a second repo: a new one starts from a walking skeleton, an existing one gets its
   services, ports and seed data described once. On each model upgrade, run a pruning pass that removes harness pieces
   no longer earning their cost.
   - *Exit:* a chunk lands in another repo without any Paja-specific change to that repo.

## First release

Before Paja Desk's phone exit (step 6) and before step 7's full scope, v0.1 ships Paja for other people's projects:

- any language, not only Rust;
- many repositories, not one;
- `paja init` to add Paja's files to a project and `paja doctor` to check the setup;
- safe defaults, so a fresh install is fail-closed;
- usage pacing inside the account's limits;
- Paja Desk as a desktop app;
- prebuilt binaries;
- a quickstart.

## Status

Steps 1 and 2 are done; step 3 is in progress.

Step 1's exit test is `evals/step1-exit.py`: headless Claude Code, in a fresh environment made from
`origin/main` and guided only by `CLAUDE.md`, must run fmt, clippy and the tests, each passing on its first command and
without editing any file. Clippy and the tests both compile Paja first, so they cover the build. The verdict comes from
a log of the cargo processes Claude really started and a re-run of the checks, never from the model's own report. It
needs a signed-in `claude`, so it runs on the owner's machine, not in CI; its latest passing output is in the pull
request that added it. Step 2 is done: `paja mcp`, Jono's import and `paja run` have landed. Step 3 is in progress:
Paja's own changes now go through the queue.
