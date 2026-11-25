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
paja run --every 5
```

The clone can go anywhere. Keep Paja's home in its default place (`.paja` in your user profile): Paja keeps the
deny-list hook's binary there, and other accounts must not be able to write to it. A drive without permissions, such
as FAT32 or exFAT, won't work.

## Linux and macOS

Install Git, rustup (<https://rustup.rs>), the GitHub CLI and Claude Code, sign in to `gh` and `claude`, then:

```sh
git clone https://github.com/eppu-peltonen/paja ~/paja && cd ~/paja
cargo install --path crates/paja
paja run --every 5
```

## Using Paja from Claude Code

The Claude Code plugin in `plugin/` starts `paja mcp`, so Paja's core shows up as MCP tools, and adds three skills:
`/paja:status` (what is queued, active or waiting on you), `/paja:answer` (answer a question Paja asked on an issue and
queue the item again) and `/paja:spec` (turn the conversation into an issue labelled `paja`, shown to you to approve
first). With `paja` installed on `PATH` as above, install the plugin with one command, the same on Windows, Linux and
macOS:

```sh
claude plugin install paja --marketplace eppu-peltonen/paja
```

Inside Claude Code, `/plugin install paja --marketplace eppu-peltonen/paja` does the same. Start Claude Code from inside
the repository's checkout, because the MCP tools work on the checkout they run in.

## What happens next

`paja run --every 5` takes the oldest open issue labelled `paja`. It builds the change, runs Vasara's checks, has it
reviewed and lands it, or asks a question on the issue when it can't decide. Then it takes the next issue. When the
queue is empty it checks again every five minutes. Stop it with Ctrl+C. After pulling a newer Paja, run
`cargo install --path crates/paja` again.
