# Running Paja safely on your machine

Read this before your first `paja run` on a computer that holds anything you care about. Setup is in
[`setup.md`](./setup.md); the full design is in [`design/architecture.md`](./design/architecture.md).

## What the agents can and can't do

Every agent Paja starts (builder and reviewers) runs under these limits. Claude Code's auto mode is the base layer: its
safety improvements arrive with Claude Code updates, so keep Claude Code current. The deny-list hook only adds to it.

- **The deny-list hook.** A thin extra layer for what only Paja knows (no `git push`, the programs in `.paja/commands`,
  fail closed); it is not a sandbox and does not replace auto mode. Each tool call goes to `paja hook`, which allows the file tools, local `git` and the programs
  in `.paja/commands` (`cargo` when the file is missing), in plain shapes, and blocks the rest. It fails closed. Reviewers
  run it read-only. No settings file or MCP server loads, so neither your settings nor a file on the branch can loosen it.
- **`--tools`.** An agent is given only the built-in tools its role needs. There are no web or subagent tools.
- **Auto mode.** Claude Code always starts with `--permission-mode auto`, never a weaker mode. A Claude Code that rejects
  it stops the step with an error telling you to update it.
- **Turn caps.** Each session has `--max-turns` (80 for builders, 30 for reviewers, set per role in `.paja/models`), so
  an agent going in circles can't burn your usage.
- **Checks come from the default branch.** `.paja/checks`, `.paja/commands`, `.paja/models` and the rest of `.paja/`
  are read from `main`, never from the branch under review, so a change can't edit the checks that judge it.
- **Review before landing.** A reviewer that didn't write the change must approve it, and CI must be green (high-risk
  changes need two reviewers).

**Without a sandbox, code an agent writes runs as you when the checks run.** The hook is a guardrail, not a sandbox.
Tests and build scripts on the branch run as your user, with your files and your sign-ins (for example credential files
in your home folder). Paja detects changes to git's configuration and hooks and stops (see below), but it cannot see
everything such code does. `paja doctor` warns while there is no sandbox, and `paja run` says so before it starts.

## The sandbox

When [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) (`sbx`) is installed, `paja run` (and `paja run
--workers`, `paja triage` and `paja init`'s agents) runs every agent and every check inside one sandbox named `paja`.
You don't set anything: Paja finds `sbx` on your `PATH`. Nothing requires Docker either; without `sbx`, agents and
checks run on your machine as described above.

1. Install Docker Sandboxes (`sbx`). Before a run uses it, Paja checks that `sbx version` and `sbx ls` work (`sbx ls`
   needs the Docker engine), for at most about 10 seconds. If they don't, `paja run` stops with an error that says to
   start Docker Desktop. It never falls back to running on your machine, then or later in the run.
2. Sign in to Claude Code once inside the sandbox: `sbx exec -it paja claude`, then `/login`. (The first run creates
   the sandbox; a run whose agent finds it isn't signed in parks the item with this command in the question.)
3. If the project's checks need tools, put a POSIX shell script in `.paja/setup` on the default branch. Paja runs it in
   the sandbox once, and again when it changes.

`PAJA_SBX` overrides the choice. `PAJA_SBX=off` (or an empty value) always runs on your machine, without asking `sbx`
anything. `PAJA_SBX=<program>` names the `sbx` program to use, as a path or a name on `PATH`; then a sandbox that can't
start stops the run in the same way. On Windows, PowerShell: `$env:PAJA_SBX = "off"`.

`paja doctor` and the first progress line of `paja run` say which way agents run and why: `sbx` found on `PATH`,
`PAJA_SBX` set, `PAJA_SBX` off, or no `sbx` found. Each `run.started` event in the log records it too, as `backend`
(`sandbox` or `host`) and `backend_why` (`detected`, `set`, `off` or `not_found`).

What it adds: code only reaches the worktrees and the repository's `.git` directory. Your home folder, credential files,
global git configuration, Paja's event log and the hook's binary are not in it, and neither are your environment
variables. If a run says the sandbox lacks a directory, remove it with `sbx rm paja`; the next run makes it again.

## A GitHub token for Paja only

`gh` signs in as you, with all your repositories. Give Paja a narrower one instead:

1. On GitHub, go to Settings, Developer settings, Personal access tokens, Fine-grained tokens, and generate one.
2. Under Repository access choose **Only select repositories**, and pick the one repository.
3. Repository permissions: **Contents**, **Pull requests** and **Issues** read and write; **Workflows** read. Leave the
   rest at no access. Pick a short expiry.
4. In the shell that runs `paja run`, set `GH_TOKEN`; `gh` uses it over its stored sign-in, in that shell only:

```sh
export GH_TOKEN=<the token>
paja run --every 5
```

In PowerShell: `$env:GH_TOKEN = "<the token>"`. Your other terminals keep your own sign-in. Check with `gh auth status`
and `paja doctor`. Paja needs the token to merge pull requests, so the repository's settings must let it; if you protect
the default branch, a token cannot get around that. Don't write the token in a file in the repository.

## Stopping and undoing

- **Stop a run:** Ctrl-C. The first lets the step in flight finish and then stops; the item stays active and the next
  `paja run` resumes it. A second Ctrl-C ends the agent or check at once.
- **The tamper stop.** If an agent or a check changed git's configuration or hooks in the repository, Paja records
  `run.tampered`, runs no git or gh there and every later command on that repository refuses. The error names the files
  that changed. Look at them, restore them (`.git/config`, hooks and the like), then clear the stop:

  ```sh
  paja run --tamper-checked
  ```

  See [`troubleshooting.md`](./troubleshooting.md).
- **Undo a landed change.** Each change lands as one squashed pull request. Open the merged pull request on GitHub and
  press **Revert**, which opens a new pull request that undoes it. Or, in a clone, `git revert <commit>` the change's
  commit on the default branch and push it through a pull request. Paja never rewrites history.

## What Paja sends where

- **To Claude, through Claude Code:** the issue's text, the code in the worktree, tool output and the diffs reviewers
  read. Paja has no other model connection; your Claude Code account's terms apply.
- **To GitHub, through `gh`:** the branch, pull requests, comments, review results, escalation questions and follow-up
  issues. **In a public repository all of these are public.** Don't put secrets in issues, and don't point Paja at a
  private repository you wouldn't want its text copied from.
- **To a notification URL,** only if you set `PAJA_NOTIFY`: the repository and the issue's number and title.

Paja's own state (the event log) stays in `PAJA_HOME` on your machine.
