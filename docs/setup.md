# Setting up Paja

How to install Paja and start it on its own queue. Paja runs natively on Windows, Linux and macOS. Before the first
run, read [`safety.md`](./safety.md): what the agents can do on your machine, the optional sandbox and a GitHub token
limited to one repository.

## What Paja needs

- **Git.** On Windows, Git for Windows, whose shell runs Vasara's checks.
- **The GitHub CLI** (`gh`), signed in to an account that can push to the repository and merge its pull requests.
- **Claude Code**, signed in. On Windows use the native `claude.exe` from the installer below, not the npm package.
- **Paja itself:** a prebuilt `paja` from a GitHub release, or built from source with Rust.

## Install a release

Each release on <https://github.com/eppu-peltonen/paja/releases> has a `paja` archive per platform and a
`SHA256SUMS` file: `x86_64-pc-windows-msvc` (`.zip`), `aarch64-apple-darwin` (Apple silicon), `x86_64-apple-darwin`
(Intel Macs) and `x86_64-unknown-linux-gnu` (`.tar.gz`). Each archive holds `paja` (`paja.exe` on Windows), the
license, the README and the changelog.

On Windows, in PowerShell, install the other tools, then put `paja.exe` in a folder on your `PATH`:

```powershell
winget install Git.Git GitHub.cli
irm https://claude.ai/install.ps1 | iex
$v = (irm https://api.github.com/repos/eppu-peltonen/paja/releases/latest).tag_name
$name = "paja-$($v.TrimStart('v'))-x86_64-pc-windows-msvc"
$dir = Join-Path $env:LOCALAPPDATA "Programs\paja"
irm "https://github.com/eppu-peltonen/paja/releases/download/$v/$name.zip" -OutFile "$env:TEMP\$name.zip"
irm "https://github.com/eppu-peltonen/paja/releases/download/$v/SHA256SUMS" -OutFile "$env:TEMP\SHA256SUMS"
$want = ((Get-Content "$env:TEMP\SHA256SUMS" | Where-Object { $_ -like "* $name.zip" }) -split '\s+')[0]
$got = (Get-FileHash "$env:TEMP\$name.zip" -Algorithm SHA256).Hash
if ($want -and $got -eq $want) {
  Expand-Archive "$env:TEMP\$name.zip" $env:TEMP -Force
  New-Item -ItemType Directory $dir -Force | Out-Null
  Copy-Item "$env:TEMP\$name\paja.exe" $dir
  $path = [Environment]::GetEnvironmentVariable("Path", "User")
  if (($path -split ';') -notcontains $dir) { [Environment]::SetEnvironmentVariable("Path", "$path;$dir", "User") }
} else { Write-Error "$name.zip doesn't match SHA256SUMS; download it again, and don't install it" }
```

The `if` block installs `paja.exe` only when the archive's checksum matches the release's `SHA256SUMS`, and adds its
folder to your `PATH` only once, so an update can run the same lines.

On macOS or Linux, install Git, the GitHub CLI and Claude Code, then (with the archive for your machine):

```sh
v=$(curl -fsSL https://api.github.com/repos/eppu-peltonen/paja/releases/latest | sed -n 's/.*"tag_name": *"\(.*\)".*/\1/p')
name="paja-${v#v}-aarch64-apple-darwin"   # or x86_64-apple-darwin, x86_64-unknown-linux-gnu
curl -fsSLO "https://github.com/eppu-peltonen/paja/releases/download/$v/$name.tar.gz"
curl -fsSLO "https://github.com/eppu-peltonen/paja/releases/download/$v/SHA256SUMS"
grep " $name.tar.gz\$" SHA256SUMS | shasum -a 256 -c - \
  && tar -xzf "$name.tar.gz" && mkdir -p ~/.local/bin && cp "$name/paja" ~/.local/bin/
```

The last command installs `paja` only when its checksum matches. Where `shasum` is missing (some minimal Linux
installs), use `sha256sum -c -` in its place.

`~/.local/bin` must be on your `PATH`. Open a new terminal (on Windows, a new PowerShell window), sign in with
`gh auth login` and `claude`, and check with `paja --version`, which names the version and the commit it was built
from. To update, install the newer release the same way.

## Build from source

Contributors, and anyone who wants Paja's latest `main`, build it with Rust, through rustup. On Windows, Rust also
needs Microsoft's C++ build tools, which provide the linker (`link.exe`).

### Windows

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

### Linux and macOS

Install Git, rustup (<https://rustup.rs>), the GitHub CLI and Claude Code, sign in to `gh` and `claude`, then:

```sh
git clone https://github.com/eppu-peltonen/paja ~/paja && cd ~/paja
cargo install --path crates/paja
paja run --once
```

Then `paja run --every 5` keeps going, as described below.

## Setting up your own repository

In a checkout of an existing repository, with `gh` signed in and the default branch pushed:

```sh
paja init          # --ci also writes a GitHub Actions workflow when the repository has none
```

### Detection

`paja init` runs one agent, on the `init` role in `.paja/models`, to detect the project, whatever its languages. The
agent reads the default branch in a temporary worktree, under the same deny-list hook and read-only tools as
`paja triage`, and treats the repository's text (READMEs, scripts, workflows) as data to judge, never as instructions.
It returns each stack with its directory (so monorepos and workspaces work), its checks, the programs they need, the CI
setup steps, the high-risk paths, the toolchain versions it finds in files such as `rust-toolchain.toml`, `.nvmrc`,
`.python-version`, `.tool-versions` and `global.json`, and the services in compose files. When `.github/workflows/`
already runs test, lint or format commands, it proposes those first, since the project already trusts them. Every item
names the file that backs it.

Paja keeps only what it can back up: the named file must exist on the default branch, and each check's program must be
one of the proposal's commands (a check is also one plain command, with no `;`, `|`, `&`, redirects or `$`). Everything
else is dropped. A stack in a subdirectory gets `cd <dir> && ` in front of its checks. The pull request body lists what
the agent found, each check with the file it comes from, the environment, and what was dropped, so you review it there
before merging.

A repository with no `init` line in `.paja/models` runs the agent on the account's default model. `paja init --no-agent`
skips the agent and uses a small built-in table instead (`Cargo.toml`; `package.json` with its `test` and `lint`
scripts and the package manager its lockfile shows, npm, pnpm or yarn; `pyproject.toml` or `requirements.txt`; `go.mod`).
The table is also the fallback when `claude` is missing or not signed in, or when the agent's answer is malformed or
holds nothing the files back up; `paja init` then says so in its output (`note`, and `detected.fallback` in `--json`).

`paja init` writes, in one pull request from the branch `paja/init`:

- `.paja/checks` and `.paja/commands` for what it found. When it finds nothing, both hold commented templates
  and `paja init` says to fill in the checks in the pull request.
- `.paja/models` with Paja's defaults, and `.paja/high-risk` listing `.paja/`, `CLAUDE.md`, `.github/`, `.claude/` and
  the stack's build configuration.
- `CLAUDE.md`: a starter with the project's commands and the rules for agents, or, when the repository has one, a
  "Working with Paja" section appended to it, the rest untouched.
- With `--ci` and no workflow in `.github/workflows`, a workflow that runs the checks on pull requests; without
  `--ci`, `.paja/ci` containing `none`, which says the repository has no CI (see [CI](#ci)).

It also creates the work and follow-up labels (`.paja/labels`, `factory` and `factory-follow-up` when missing) if the
repository lacks them. It works in a temporary worktree, so your checkout is untouched, and it pushes only `paja/init`,
never the default branch. Read the pull request, change what is wrong, and merge it: Paja starts working once it is
merged, on issues with the work label.

If `.paja/checks` is already on the default branch, `paja init` refuses; `--force` writes the files again on the branch.
`--json` prints what it detected, the files it wrote and the pull request, and the MCP tool `init` (which takes
`no_agent`) returns the same. `detected` holds `stacks`, `checks` (each `{command, source, file}`, `source` being
`agent` or `table`), `commands`, `environment` (`stacks` with each one's `dir`, `toolchains`, `services`), `dropped` (each
`{item, reason}`) and `fallback`.

## Starting a new project

For a project that doesn't exist yet, with `gh` signed in:

```sh
paja init --new my-app --stack rust     # or node, python, or any other stack; --public for a public repository (default --private)
```

Without `--stack`, on a terminal, `paja init --new my-app` asks on stdin what you are building and whether you prefer a
language or framework, shows the recommendations (why, trade-offs and the kind of skeleton) and takes a number or a
stack name. With `--json`, or with no terminal, `--stack` is required and the error says to run
`paja init --recommend` first:

```sh
paja init --recommend "a reading list web app" [--prefer rust]...
```

An agent on the `init` role, with no repository to read, answers `{recommendations: [{stack, why, tradeoffs,
skeleton}], questions}`: one to three options, best first. Stated preferences win unless one is a poor fit for what you
are building, and then `why` says so. `skeleton` is `built-in` for `rust`, `node` and `python`, and `generated`
otherwise (see below). The MCP tool `init_recommend` (`building`, `prefer`) returns the same JSON.

`--new` creates the repository `my-app` under your account with `gh repo create`, clones it to `my-app` in the current
directory (`--dir <path>` to clone elsewhere), and pushes one first commit holding a walking skeleton:

- a minimal app with one passing test, with a formatter and a linter configured (Rust: `rustfmt` and `clippy`; Node:
  Prettier and ESLint, run by `npm run lint`; Python: `ruff` for both, with `pytest` for the test);
- a workflow in `.github/workflows/paja.yml` that runs the checks on pull requests;
- the `.paja/` files and `CLAUDE.md` that `paja init` writes for the stack, and `.paja/labels` naming the work label
  `paja` and the follow-up label `paja-follow-up`, which it creates in the repository.

This is the one place Paja pushes to a default branch: the repository is empty and new. The rust, node and python
skeletons are templates in the binary (`crates/paja/templates/`), the fast path, so for them the command needs the
network only for GitHub (and for installing the stack's tools in CI). When it succeeds it prints the next steps: file an
issue with `/paja:spec`, then `paja run --once`. If it fails after creating the repository, the error names the
repository so you can finish by hand or delete it. `--json` prints the repository, the directory, the stack, the
`skeleton` kind (`built-in` or `generated`), the files, the checks and the next steps; the MCP tool `init` takes `new`,
`stack`, `public` and `dir` for the same.

### Generated skeletons

`--stack` takes any language or framework (`go`, `ruby`, `elixir`, `C#`, ...; letters, digits, spaces and `-_.+#/`).
For one with no template, an agent on the `init` role writes the skeleton instead, in the fresh clone:

1. A first session, with no repository and no tools, names the programs the stack's build, formatter, linter and test
   runner run. Paja keeps plain program names and drops shells, programs that run another command (`nohup`, `wsl`,
   `busybox`), network tools and the like, with or without an extension (`bash.exe` and `cmd.bat` are dropped too).
2. A second session writes a minimal app, one passing test, the formatter's and linter's configuration and a
   `.gitignore`, and runs the formatter, linter and test until they pass. It runs under the deny-list hook, and the
   only programs it may run besides local git are the ones from step 1. It can't write `.github/` (the hook refuses CI
   configuration), and it doesn't commit: if it commits or switches branches anyway, Paja stops and pushes nothing.
3. Paja detects the stacks and checks of the result with the same `init` agent that `paja init` uses, writes `.paja/`,
   `CLAUDE.md` and the workflow in `.github/workflows/paja.yml` from that, and runs every line of `.paja/checks`
   locally.
4. Only when every check passes does it make the first commit and push, as it does for a built-in skeleton.

When a check fails, or the detection finds no check, nothing is committed or pushed: the repository exists on GitHub and
the clone stays with the files, the error names the check that failed (with its output) and says so, and you fix it in
the clone and commit and push by hand, or delete the repository. The three agent sessions each spend usage on the `init`
model of `.paja/models` (the account's default here, as the new repository has no such file yet). With a sandbox
(`PAJA_SBX`) the agent and the checks run in it, with the clone mounted. Set one for a generated skeleton when you can:
the stack's own programs are often interpreters or runners (`python`, `node`, `npx`, `ruby`) that run any code they are
given, so without a sandbox the hook can't keep the agent from running arbitrary code as you.

## Check the setup

`paja doctor` (with `--repo <path>`; the default is the current directory) checks that this machine and the repository
are ready, so you find out what is missing before `paja run` fails partway. It changes nothing in the repository. Each
check is `ok`, `warning` or `failed`, and each warning or failure says how to fix it:

- `git` runs; on Windows, Git for Windows' `sh` is found.
- `gh` runs, is signed in (`gh auth status`), and the account can push and merge in the repository (`viewerPermission`
  is `WRITE`, `MAINTAIN` or `ADMIN`).
- `claude` runs and is signed in (`claude auth status`). It is a warning when Claude Code is older than the latest
  release (`claude update` fixes it): agents get Anthropic's safety updates only through Claude Code updates. The
  lookup takes a few seconds at most; when it fails (offline), the check says so and stays ok. `PAJA_CLAUDE_LATEST`
  names another source: an `http(s)` URL or a file holding the version.
- The deny-list hook passes its self-test, and Paja's home folder (`PAJA_HOME`) is private to your user.
- No environment is left over from a closed item. A parked item keeps its environment (worktree and ports) until it
  is answered; if its issue is closed instead, this is a warning that names the environment and the
  `paja env remove <name>` that frees it (the branch is kept).
- Claude Code's results report the usage window. Paja reads the window from the `rate_limit_event` messages of
  `claude -p --output-format stream-json`; when none of the latest five agent steps in the log reported one, this is a
  warning, because a nearly used window then can't hold a build back. `claude update` fixes an old Claude Code; with
  an API key there is no window, and the warning is expected. Before any step has run, it is ok.
- The repository has an `origin` on GitHub, the work label (`.paja/labels`) exists, `.paja/checks` on the default
  branch has commands, and CI is set up (a workflow exists) or `.paja/ci` says `none`. These read the default branch
  as your checkout has it (`origin/main`, else `main`); push first, or `paja run` will not see what you changed.
- `.paja/models` on the default branch parses. It is a warning when a line names a role this Paja doesn't know: that
  line is skipped, so the role runs on the default model or turn cap. Install the newer Paja, or fix the typo.
- The sandbox: a warning, not a failure, when `PAJA_SBX` is not set. Agents and checks then run as your user, with
  your files and sign-ins, behind the deny-list hook only.

With `--json` it prints `{"checks": [{"name", "status", "detail", "fix"}], "ok": bool}`; `fix` is null for a passing
check, and `ok` is false when any check failed (a warning doesn't count). The exit code is non-zero when any check
failed. The MCP tool `doctor` returns the same JSON.

### How the safety layers fit

- **Base layer: Claude Code's auto mode.** Every agent Mestari starts runs under `--permission-mode auto`. Its permission
  checks, and the improvements Anthropic ships to them, reach Paja's agents through Claude Code updates, so keep Claude
  Code current.
- **On top: the deny-list hook** (`paja hook`). A thin extra layer for what only Paja knows: no `git push`, only the
  programs in `.paja/commands`, and fail closed. It is not a sandbox and does not stand in for auto mode.
- **Optional: the sandbox** (`PAJA_SBX`, Docker Sandboxes). Recommended when people outside your team can write the
  repository's issues or code, and for long unattended runs (`paja run --every`, `--workers` pools). Nothing requires
  Docker; see [`safety.md`](./safety.md).

## Using Paja from Claude Code

The Claude Code plugin in `plugin/` starts `paja mcp`, so Paja's core shows up as MCP tools, and adds five skills:
`/paja:init` (set up the repository, or start a new project, without the terminal: it runs `doctor`, then `init`, shows
what was detected and the files, and links the pull request), `/paja:doctor` (check readiness and explain each failure
with its fix), `/paja:status` (what is queued, active or waiting on you), `/paja:answer` (answer a question Paja asked
on an issue and queue the item again) and `/paja:spec` (turn the conversation into an issue with the work label, shown
to you to approve first). With `paja` installed on `PATH` as above, install the plugin with one command, the same on
Windows, Linux and macOS:

```sh
claude plugin install paja --marketplace eppu-peltonen/paja
```

Inside Claude Code, `/plugin install paja --marketplace eppu-peltonen/paja` does the same. Start Claude Code from inside
the repository's checkout, because the MCP tools work on the checkout they run in.

## CI

Paja waits for the pull request's CI before it lands. `.paja/ci`, read from the default branch next to `.paja/checks`,
says whether the repository has any: `required` (the default when the file is missing) or `none`. With `required`, a
repository with no active GitHub Actions workflow (`gh workflow list` is empty) stops the run before it claims anything,
once an item is ready to take (a pass with nothing to take doesn't call GitHub for this), and gives an error that names the
two fixes: add a workflow that runs the checks on pull requests, or put `none` in `.paja/ci` on the default branch. The
check only sees that a workflow exists, not that it runs on pull requests: a repository whose workflows run only on push
or on a schedule passes it and then waits five minutes for checks that never appear. With `none`, Paja lands on Vasara's
checks and the reviewers alone, and says so in the pull request body. If the default branch's protection still requires
status checks, GitHub refuses the merge and the error says so.

## Paja Desk

Paja Desk shows the queue of every repository Paja knows in one window, with what waits on you first, and follows the
event log as it changes. You can answer Paja's questions there and file new work (it shows the issue for approval
before filing it). Its footer shows Desk's version and the `paja` it found, and it warns when the two differ.

Under **Repositories** Desk lists every repository Paja knows. Add one by pasting its checkout folder's path (Desk
syncs its issues there, as `paja work sync` does, and remembers the folder). For each repository:

- **Check** shows `paja doctor`'s result with the fix for each problem. Desk won't start a run while a check fails.
- **Run one item** starts `paja run --once --yes` in the checkout, and **Keep running** starts `paja run --every 5`.
  It is the installed `paja`, so the run is exactly the CLI's, with its own lock and hook. Its output shows under the
  repository and is kept in `desk/<owner>@<name>.log` in Paja's home.
- **Stop after this step** and **Stop now** do what the first and second Ctrl-C do in a terminal.
- A run started in a terminal shows as running too, and Desk won't start a second one in the same repository; stop it
  where it was started.
- **Pool** opens the repository's worker pool: whether it runs, how long, what it spent and landed, how many items
  are ready or held for your answer, which lane is landing and which wait to land, a row per lane (item, stage,
  step, time in step, spend, the files it was admitted for), the waiting items and why, the last landings and the
  pool's latest events. **Start the pool** starts `paja run --every 5 --workers <N>`
  with the lane count in the box. **Set lanes** changes the count of a running pool, which it reads on its next tick,
  as `paja pool lanes <N>` does; lowering it says which busy lanes finish their items first. A pool started in a
  terminal shows here too, and its lanes can be changed the same way.

Closing Desk while its runs are going asks whether to stop them or leave them running on their own.

### Install Paja Desk

Each release also has Desk's installers, listed in the same `SHA256SUMS`. Each one carries the matching `paja`, so
installing Desk is enough to use the CLI and the Claude Code plugin too:

- **Windows:** `paja-desk_<version>_x64-setup.exe` installs for your user only and puts its folder, which holds
  `paja.exe`, on your `PATH` (uninstalling takes it off again). Open a new terminal to use `paja`. The installer isn't
  signed yet, so SmartScreen may stop it: choose **More info**, then **Run anyway**. Desk uses WebView2, which
  Windows 10 and 11 already have.
- **macOS:** `paja-desk_<version>_aarch64.dmg` (Apple silicon) or `_x64.dmg` (Intel). Drag Paja Desk to
  Applications. The app isn't signed yet, so open it the first time by right-clicking it in Applications and choosing
  **Open**, then **Open** again (on newer macOS, if that isn't offered: System Settings, Privacy & Security,
  **Open Anyway**). To use the `paja` inside it from a terminal:
  `sudo ln -sf "/Applications/Paja Desk.app/Contents/MacOS/paja" /usr/local/bin/paja`.
- **Linux:** `paja-desk_<version>_amd64.deb` installs `paja-desk` and `paja` in `/usr/bin`
  (`sudo apt install ./paja-desk_<version>_amd64.deb`). The `.AppImage` runs without installing
  (`chmod +x` it first), but its `paja` stays inside it, so install `paja` from its archive as well.

### Run Desk from source

From a clone of Paja:

```sh
cargo run --release -p paja-desk
```

On Linux it needs WebKitGTK's development packages first (on Debian or Ubuntu:
`sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev`). It reads the same `PAJA_HOME` as `paja`.

## Notifications

Paja tells you when an item needs your answer, a pass of `paja run --every` fails, the loop stops, a change lands
with review findings still open, listed in its pull request, or `.paja/models` names a role this Paja doesn't know
(once per role). Run in a terminal, it shows a desktop notification; set `PAJA_NOTIFIER=desktop` to get one without a
terminal, `PAJA_NOTIFIER=none` to turn them off, or point it at a program of your own, which gets the title and the
text as two arguments. To hear about it on your phone, put an `https://` URL, such as an ntfy topic, in `PAJA_NOTIFY`
or as the one line of `.paja/notify` on the default branch; Paja POSTs a short text there with `curl`: the repository,
the issue's number and title (for a skipped role, the default branch and the role names from `.paja/models`, escaped
and cut short), never the question or any local detail. A notice that can't be shown is reported once on stderr and
the run carries on. An issue waiting for your answer also carries the
`needs-answer` label of `.paja/labels` (`factory-needs-answer` by default) until you answer, so you can filter or
subscribe to it on GitHub.

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

To work several issues at once, give the loop lanes: `paja run --every 5 --workers 2` keeps up to two items going,
each in its own environment, and starts the next one as soon as a lane is free. Change the number while it runs with
`paja pool lanes 3` (lowering it lets the running items finish first), and see the lanes, the stage and step each is
in, what it has spent and what is waiting with `paja pool status`. Every lane uses your Claude subscription at the
same time, so more lanes reach its limits sooner.

Lanes never work on the same files at once. Before an item gets a lane, Paja plans which files it will touch: a
`Touches: src/a.rs, docs/` line in the issue names them, and otherwise a short, cheap agent (the `plan` role in
`.paja/models`) reads the code and lists them. An item that overlaps a running one waits, and `paja pool status` says
which one; two high-risk items never run at once. `paja pool plan <number>` shows an item's plan.

Stop it with Ctrl+C. The first Ctrl+C lets the step in flight (an agent or a check) finish and keeps it, then stops; the
item stays active, and the next `paja run` resumes it where its environment left off. Press Ctrl+C again to stop at
once: Paja ends the agent or check and everything it started, and the item still resumes next time, without that
step's unfinished work. Closing the terminal is like the second Ctrl+C; after a reboot the item resumes the same way.

To update, install the newer release, or pull `main` and run `cargo install --path crates/paja` again.

Something went wrong, or you want to update or uninstall Paja: see [`troubleshooting.md`](./troubleshooting.md).
