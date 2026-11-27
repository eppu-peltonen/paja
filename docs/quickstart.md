# Quickstart: Paja on your own project

This page takes you from nothing to a change Paja built and landed in your own repository. It needs about half an
hour, most of it waiting for the first run. Before the first run, read [`safety.md`](./safety.md): what the agents
can do on your machine, and how to limit and undo it.

## 1. Install

You need Git, the GitHub CLI (`gh`), Claude Code and Paja. Paja comes either with Paja Desk's installer, which also
puts the `paja` command on your `PATH`, or as the `paja` binary alone.

**Windows**, in PowerShell:

```powershell
winget install Git.Git GitHub.cli
irm https://claude.ai/install.ps1 | iex
```

Then run Desk's installer, `paja-desk_<version>_x64-setup.exe` from the
[latest release](https://github.com/eppu-peltonen/paja/releases/latest), and open a new PowerShell window. Windows
may stop the unsigned installer: choose **More info**, then **Run anyway**.

**macOS**: install Git (`xcode-select --install`), the GitHub CLI (`brew install gh`) and Claude Code
(`curl -fsSL https://claude.ai/install.sh | bash`). Then open `paja-desk_<version>_aarch64.dmg` (Apple silicon) or
`_x64.dmg` (Intel), drag Paja Desk to Applications, open it the first time by right-clicking it and choosing
**Open**, and put its `paja` on your `PATH`:

```sh
sudo ln -sf "/Applications/Paja Desk.app/Contents/MacOS/paja" /usr/local/bin/paja
```

**Linux** (Debian or Ubuntu): install Git, the GitHub CLI and Claude Code, then Desk's package, which puts `paja` in
`/usr/bin`:

```sh
sudo apt install ./paja-desk_<version>_amd64.deb
```

To install only the `paja` binary (no Desk), or to build from source, see [`setup.md`](./setup.md). Check with:

```sh
paja --version
```

## 2. Sign in and check the machine

```sh
gh auth login
claude
paja doctor
```

`claude` asks you to sign in the first time; quit it once you are in. `paja doctor` checks Git, `gh`, `claude`, the
deny-list hook and, inside a repository, its labels and `.paja` files. Every problem comes with its fix. Outside a
repository the repository checks fail, which is expected until step 3.

## 3. Set up the project

**An existing repository**: in its checkout, run

```sh
paja init
```

It detects the stack, writes Paja's files on a branch, creates the work labels and opens one pull request. Read it
and merge it; Paja reads its files from the default branch only. Add `--ci` to also write a workflow that runs the
checks on pull requests when the repository has none.

**A new project**: `paja init --new` creates the GitHub repository, clones it and pushes a walking skeleton (a
minimal app with a passing test, a formatter, a linter, a CI workflow and Paja's files) as the first commit:

```sh
paja init --new my-app --stack node
cd my-app
```

`rust`, `node` and `python` start from a ready-made skeleton; any other stack (`--stack go`) gets one an agent writes,
and nothing is pushed unless its checks pass. The repository is private unless you pass `--public`. Not sure which
stack? Leave `--stack` out on a terminal and Paja asks what you are building and which language or framework you
prefer, shows one to three recommended stacks with their trade-offs, and takes a number or a stack name:

```sh
paja init --new my-app
```

`paja init --recommend "a reading list web app" --prefer rust` prints the same recommendations and creates nothing.

Either way the repository gets these files:

| File | What it does |
| --- | --- |
| `.paja/checks` | The commands Vasara runs on every change, such as the tests and the linter. A change lands only when they pass |
| `.paja/commands` | The programs agents may run besides local git, such as `npm` |
| `.paja/models` | The model for each agent role (builder, reviewers) |
| `.paja/high-risk` | Paths that make a change high risk, which then needs two reviewers who agree |
| `.paja/ci` | `none` when the repository has no CI, so Paja lands on its checks and reviewers alone |
| `CLAUDE.md` | What agents need to know to work in the repository, with a "Working with Paja" section |

Run `paja doctor` again in the checkout; every check should now be ok.

## 4. Write the first issue

Paja works on GitHub issues with the repository's work label: `factory`, unless `.paja/labels` names another. Write
one in any of three ways:

- In Claude Code, with the plugin installed (`claude plugin install paja --marketplace eppu-peltonen/paja`): talk
  through what you want, then `/paja:spec` writes the issue and shows it to you before filing it.
- In Paja Desk: **New work**, then review and file.
- On GitHub: a new issue with the `factory` label.

A good issue names the goal, what done looks like, and what is out of scope, and is small enough for one pull
request. For example:

```markdown
Add a /health endpoint

## What to build
GET /health answers 200 with {"status": "ok"}, so the hosting platform can check the app is up.

## Acceptance criteria
- [ ] GET /health returns 200 and {"status": "ok"}
- [ ] A test covers it

## Out of scope
Checking the database or other dependencies.
```

It is good because a reviewer can check every line of it, and nothing in it needs a decision Paja would have to
guess.

## 5. Run it

In the checkout:

```sh
paja run --once
```

Paja syncs the issues, says which item it picked and where it will work, and asks `Go ahead? [y/N]`. Then, for at
most five rounds, it prints what each step is doing:

- `round 1 of 5: Claude is building the change`: a builder agent implements the issue in its own worktree and
  commits.
- `running Vasara's checks`: the commands in `.paja/checks`, run against the commit.
- `reviewer 1 of 1 is reading the change`: an independent reviewer agent reads the diff against the issue.
- `opened <url>; waiting for CI`: the pull request is open on GitHub, and Paja waits for its checks.
- `landed #<number> (<url>)`: Paja merged it.

A failed check or any reviewer finding goes back to the builder for another round. The last line is the outcome:
landed, or escalated with a question.

## 6. When Paja asks

When a run can't decide something (reviewers disagree, five rounds weren't enough, or the issue needs a decision),
Paja escalates: it asks on the issue, labels it so it stands out, and Posti tells you: a desktop notification when
`paja` runs in a terminal (`PAJA_NOTIFIER=desktop` gets one without), or a program of your own (`PAJA_NOTIFIER`) or a push URL such as an ntfy topic for your phone (`PAJA_NOTIFY`
or `.paja/notify`); see [Notifications](./setup.md#notifications). Answer in any of three ways, and the item goes back
in the queue with your answer:

- In Paja Desk, in the item's answer box under **Waiting on you**.
- In Claude Code, with `/paja:answer`.
- In a terminal: `paja work answer <number> "<your answer>"`.

## 7. Keep going

```sh
paja run --every 5
```

This takes the next item after each one, and checks for new issues every five minutes while the queue is empty. Paja
Desk does the same with **Repositories**, then **Keep running**, and shows the queue, the run in progress and
what waits on you. Ctrl-C once stops after the current step; twice stops at once.

`paja work list` shows each item's status and what its runs spent, and `/paja:status` in Claude Code summarises the
queue, with today's usage. `paja usage` (`--item <number>`, `--since <YYYY-MM-DD>`, `--json`; MCP tool `usage`) reads
the recorded steps back: per item the rounds, steps by role and model, tokens, turns, wall time and outcome, totals
per day and overall, and the median and 90th percentile per landed item. Steps whose usage couldn't be read are
counted as unknown, never as zero. Before leaving Paja running on its own, read [`safety.md`](./safety.md) again, and see
[`troubleshooting.md`](./troubleshooting.md) when something goes wrong.
