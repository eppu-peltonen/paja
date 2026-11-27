# Troubleshooting, updating and uninstalling

The errors a newcomer is most likely to meet, with the message as Paja prints it (`<...>` stands for a value Paja fills
in) and what to do. For installing, see [`setup.md`](./setup.md).

## Errors

### `gh` is not installed, signed in, or can't reach the repository

```text
running `gh` (install the GitHub CLI and run `gh auth login`)
```

```text
`gh issue list` failed in <checkout>: <gh's message>; check `gh auth status` and that this checkout has a GitHub remote
```

Install the GitHub CLI, run `gh auth login`, and run Paja from a checkout that has a GitHub remote. Check with
`gh auth status`. The account must be able to push to the repository and merge its pull requests; if landing stops at
``gh pr merge <pr>`` with a message from GitHub about permissions or branch protection, give the account those rights (or
loosen the protection that blocks a squash merge) and answer the item, as below.

### `claude` is not installed or not signed in

```text
running `claude` (install Claude Code and sign in)
```

```text
Claude Code's sign-in was refused; run `claude` and sign in again (or check ANTHROPIC_API_KEY), then answer the item to
run it again
```

Run `claude` once in a terminal and sign in. On Windows use the native `claude.exe`, not the npm package. If
`ANTHROPIC_API_KEY` is set, check that the key is valid. Then answer the item (see "An item was escalated").

### A usage limit

```text
Claude Code hit a usage or rate limit; wait until it resets, then answer the item to run it again
```

Paja doesn't retry a limit as a new session, since it would fail the same way. Wait until the limit resets, then queue
the item again with `paja work answer <number> "<anything, for example: continue>"`.

### The hook self-test failed

```text
the deny-list hook command `<command>` failed its self-test (it must block `git push` and allow `git status`), so no
agent will start; rebuild paja from main, and check that every path in the command exists as written
```

Before each agent, Mestari checks that the deny-list hook blocks `git push` (`git commit` for a read-only agent)
and allows `git status`. Install the newer release, or run `cargo install --path crates/paja` again from an
up-to-date checkout of `main`, and make sure nothing moved or deleted the hook's copy of the binary in `$PAJA_HOME/hook`. If Claude Code never called the hook, the message begins
`Claude Code never called Paja's deny-list hook during the agent's run`; look for settings that disable hooks, such as
`disableAllHooks` in managed settings.

### `.paja/checks` is missing on the default branch

```text
.paja/checks is missing on main; add the check commands there
```

```text
.paja/checks on main has no commands; add the check commands there
```

Vasara's checks are read from the default branch, never from the branch under review. Commit a `.paja/checks` file there
with one command per line (the commands Paja must run before a change may land), and merge it.

### The repository has no CI

```text
this repository has no active GitHub Actions workflow, so Paja would wait for CI that never comes; add a workflow that runs the
checks on pull requests, or put `none` in .paja/ci on the default branch to land on Vasara's checks and the reviewers
alone
```

Pick one of the two fixes named in the message. See "CI" in [`setup.md`](./setup.md). If a workflow exists but no check
shows up on the pull request, Paja stops with `no CI checks appeared on <pr> within 5 minutes; check the workflows`: make
the workflow run on `pull_request`.

### The tamper stop

```text
Paja stopped on <repository> because git's configuration changed during a run (<what changed>); it runs no git or gh
there until a person has checked and restored those files and runs `paja run --tamper-checked`
```

Something an agent or a check ran changed git's configuration in the repository, which could make later git commands run
code. Paja stops and runs no git or gh in that repository, not even to clean up. Look at the files the message names
(`.git/config`, hooks and the like), restore them, and then run `paja run --tamper-checked`.

### An item was escalated

```text
#<number> escalated:
Paja tried 3 rounds and the change still isn't ready. Last feedback:

<the reviewers' feedback>

Answer with what should change, or what is out of scope.
  answer: paja work answer <number> "<your answer>"
```

Three rounds is the most Paja spends on one change. Read the feedback, then answer with `paja work answer <number>
"<your answer>"` (or `/paja:answer` in Claude Code). The answer is posted on the issue, the item is queued again and its
next run gets the answer.

### The Windows home folder is on a drive without permissions

```text
<PAJA_HOME or the hook folder> <why>, so another account could change what's in it; delete it and Paja recreates it for
this user only (on a drive without permissions, such as FAT32 or exFAT, set PAJA_HOME to a folder on an NTFS drive
instead)
```

Paja keeps the deny-list hook's binary in its home, and no other account may be able to write there. Delete the folder
the message names so Paja recreates it for you alone, or set `PAJA_HOME` to a folder on an NTFS drive.

## Updating

From a release: download the new release and replace the `paja` binary. From source: pull `main` and run
`cargo install --path crates/paja` again. Then update the plugin with `claude plugin update paja`. Nothing else needs
doing: the event log (`$PAJA_HOME/events.jsonl`) carries over, and a newer Paja reads older logs.

## Uninstalling

1. Stop any running `paja run`. Press Ctrl+C.
2. Remove the worktrees Paja made (see below) while the binary is still installed.
3. Remove the binary: `cargo uninstall paja` if you installed from source, or delete the downloaded file.
4. Remove Paja Desk by uninstalling the app the way your system uninstalls apps.
5. Remove the plugin: `claude plugin uninstall paja`.
6. Delete `$PAJA_HOME` (`.paja` in your user profile unless you set it). It holds the event log, the hook's copy of the
   binary and the environments under `envs/`. **Deleting it loses the history**: what was queued, landed or escalated,
   and what each step spent. Keep `events.jsonl` if you may want that history later.

### Worktrees Paja left behind

Each environment is a git worktree under `$PAJA_HOME/envs/<name>`, on a branch `paja/<name>`. An interrupted run leaves
its worktree for the next run to resume. To clean up:

```sh
paja env list
paja env remove <name>
```

`paja env remove` removes the worktree and keeps its branch (delete the branch with `git branch -D paja/<name>` if you
don't want it). It refuses a worktree with uncommitted changes; `paja env remove <name> --force` discards them.
