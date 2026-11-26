# Setting up Paja

How to install Paja and start it on its own queue. Paja runs natively on Windows, Linux and macOS.

## What Paja needs

- **Git.** On Windows, Git for Windows, whose shell runs Vasara's checks.
- **Rust**, through rustup, to build Paja. On Windows, Rust also needs Microsoft's C++ build tools, which provide the
  linker (`link.exe`).
- **The GitHub CLI** (`gh`), signed in to an account that can push to the repository and merge its pull requests.
- **Claude Code**, signed in. On Windows use the native `claude.exe` from the installer below, not the npm package.

## Windows

In PowerShell:

```powershell
winget install Git.Git GitHub.cli Rustlang.Rustup
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
irm https://claude.ai/install.ps1 | iex
```

The build tools are needed even when rustup doesn't ask for them. Without them `cargo install` fails with
``linker `link.exe` not found``. Their install takes several minutes.

Then open a **new** PowerShell window, so the new tools are on `PATH`, and run:

```powershell
gh auth login
claude          # sign in, then exit
git clone https://github.com/eppu-peltonen/paja C:\Projektit\paja
cd C:\Projektit\paja
cargo install --path crates/paja
paja run --once
```

`paja run --once` takes one item, shows what it will do, and exits when the item has landed or been escalated. When
you are comfortable with that, `paja run --every 5` keeps going.

The clone can go anywhere. Keep Paja's home in its default place (`.paja` in your user profile): Paja keeps the
deny-list hook's binary there, and other accounts must not be able to write to it. A drive without permissions, such
as FAT32 or exFAT, won't work.

## Linux and macOS

Install Git, rustup (<https://rustup.rs>), the GitHub CLI and Claude Code, sign in to `gh` and `claude`, then:

```sh
git clone https://github.com/eppu-peltonen/paja ~/paja && cd ~/paja
cargo install --path crates/paja
paja run --once
```

Then `paja run --every 5` keeps going, as described below.

## Check the setup

`paja doctor` (with `--repo <path>`; the default is the current directory) checks that this machine and the repository
are ready, so you find out what is missing before `paja run` fails partway. It changes nothing in the repository. Each
check is `ok`, `warning` or `failed`, and each warning or failure says how to fix it:

- `git` runs; on Windows, Git for Windows' `sh` is found.
- `gh` runs, is signed in (`gh auth status`), and the account can push and merge in the repository (`viewerPermission`
  is `WRITE`, `MAINTAIN` or `ADMIN`).
- `claude` runs and is signed in (`claude auth status`).
- The deny-list hook passes its self-test, and Paja's home folder (`PAJA_HOME`) is private to your user.
- The repository has an `origin` on GitHub, the work label (`.paja/labels`) exists, `.paja/checks` on the default
  branch has commands, and CI is set up (a workflow exists) or `.paja/ci` says `none`. These read the default branch
  as your checkout has it (`origin/main`, else `main`); push first, or `paja run` will not see what you changed.
- The sandbox: a warning, not a failure, when `PAJA_SBX` is not set. Agents and checks then run as your user, with
  your files and sign-ins, behind the deny-list hook only.

With `--json` it prints `{"checks": [{"name", "status", "detail", "fix"}], "ok": bool}`; `fix` is null for a passing
check, and `ok` is false when any check failed (a warning doesn't count). The exit code is non-zero when any check
failed. The MCP tool `doctor` returns the same JSON.

## Using Paja from Claude Code

The Claude Code plugin in `plugin/` starts `paja mcp`, so Paja's core shows up as MCP tools, and adds three skills:
`/paja:status` (what is queued, active or waiting on you), `/paja:answer` (answer a question Paja asked on an issue and
queue the item again) and `/paja:spec` (turn the conversation into an issue with the work label, shown to you to approve
first). With `paja` installed on `PATH` as above, install the plugin with one command, the same on Windows, Linux and
macOS:

```sh
claude plugin install paja --marketplace eppu-peltonen/paja
```

Inside Claude Code, `/plugin install paja --marketplace eppu-peltonen/paja` does the same. Start Claude Code from inside
the repository's checkout, because the MCP tools work on the checkout they run in.

## CI

Paja waits for the pull request's CI before it lands. `.paja/ci`, read from the default branch next to `.paja/checks`,
says whether the repository has any: `required` (the default when the file is missing) or `none`. With `required`, a
repository with no GitHub Actions workflow (`gh workflow list` is empty) stops the pass before an item is claimed, with an
error that names the two fixes: add a workflow that runs the checks on pull requests, or put `none` in `.paja/ci` on the
default branch. With `none`, Paja lands on Vasara's checks and the reviewers alone, and says so in the pull request body.

## What happens next

`paja run --once` takes the oldest open issue with the work label (`.paja/labels`, `paja` in this repository; or the one named with `--item <number>`) and exits when
it is done. Before the first agent starts, every `paja run` prints on stderr the repository, the item, the default
branch it merges into (by squash), whether CI is required and whether a sandbox is in use. Without a sandbox it adds
that agents and the checks run as you, with your files and sign-ins, and that the deny-list hook is a guardrail, not a
sandbox. On a terminal, `--once` then asks for confirmation; `--yes` skips the question, and `--every` and runs without
a terminal never ask. With `--json`, the report gains a `sandbox` field.

`paja run --every 5` does the same in a loop. It builds the change, runs Vasara's checks, has it
reviewed and lands it, or asks a question on the issue when it can't decide. Then it takes the next issue. When the
queue is empty it checks again every five minutes.

Stop it with Ctrl+C. The first Ctrl+C lets the step in flight (an agent or a check) finish and keeps it, then stops; the
item stays active, and the next `paja run` resumes it where its environment left off. Press Ctrl+C again to stop at
once: Paja ends the agent or check and everything it started, and the item still resumes next time, without that
step's unfinished work. Closing the terminal is like the second Ctrl+C; after a reboot the item resumes the same way.

After pulling a newer Paja, run
`cargo install --path crates/paja` again.

Something went wrong, or you want to update or uninstall Paja: see [`troubleshooting.md`](./troubleshooting.md).
