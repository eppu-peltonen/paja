---
name: init
description: Set a repository up for Paja, or start a new project, without the terminal. Use when the person wants to start using Paja in this repository, asks to set up Paja, or is in an empty folder and wants a new project.
---

# Paja init

Use only the `paja` MCP tools for Paja itself, and don't run shell commands.

1. Call `doctor`. If any check failed, walk the person through each failure and its `fix`, then call `doctor` again. Don't go on until `ok` is true. Warnings are fine to go on with; mention them.
2. Look at the folder with the file tools.
   - **Empty folder, or no repository and the person wants a new project:** offer the new-project path. Ask which stack (`rust`, `node` or `python`), the repository name, and whether it is public or private (private by default). Say that this creates a GitHub repository under their account and pushes a minimal app with a passing test, a CI workflow and the `.paja/` files as the first commit. After they confirm, call `init` with `new`, `stack`, `public` (only when they chose public) and `dir` when they named one. Show the `url`, `directory` and `next_steps`, and stop.
   - **Existing repository:** go on to step 3.
3. Tell the person what the next call does: it detects the stack, writes `.paja/checks`, `.paja/commands`, `.paja/models` and `.paja/high-risk` and `CLAUDE.md` on a new branch `paja/init`, creates the work label, and opens a pull request. Nothing reaches the default branch until that pull request is merged. If the repository has no GitHub Actions workflow, ask whether they want one that runs the checks (`ci`). If `.paja/checks` is already on the default branch, say Paja is already set up and stop, unless they ask to write the files again (`force`).
4. Call `init`. Show what was detected (`detected`: the stack, the `checks` and the `commands`) and the `files` the pull request changes, as short lists.
5. Ask whether the checks and commands look right. If not, the files are only on the `paja/init` branch, and the person's checkout is still on whatever branch it was. Ask them to run `git switch paja/init` (the branch exists locally after `init`). Then read `.git/HEAD` with the file tools: it must say `ref: refs/heads/paja/init`. If you can't confirm that, stop and don't edit anything; instead give the person the exact lines to change and let them edit the pull request on GitHub. Once confirmed, make the changes the person asks for with the file tools (one command per line in `.paja/checks`, one program per line in `.paja/commands`), and tell them to commit and push `paja/init` before they merge. Never suggest committing on the default branch.
6. End with the link in `pull_request`, and say the next step is to merge it and file work with `/paja:spec`.

Keep it short, no raw JSON.
