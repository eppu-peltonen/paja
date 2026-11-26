---
name: doctor
description: Check that this machine and repository are ready for Paja and explain each failure with its fix. Use when the person asks whether Paja is set up, why `paja run` fails, or after an error that mentions git, gh, claude, labels or the sandbox.
---

# Paja doctor

Use only the `paja` MCP tools and don't run shell commands. Change nothing.

1. Call `doctor`. It checks the checkout Claude Code runs in.
2. If `ok` is true, say so in one line and mention any warnings with their `fix`.
3. Otherwise go through each check whose status is `failed`, then each `warning`: say in plain words what it checks, what `detail` says is wrong, and the exact `fix`. Give failures first, and don't list passing checks one by one.
4. Offer to call `doctor` again once the person has applied the fixes, and say how many failures are left.

Keep it short, no raw JSON. If `doctor` itself returns an error (for example the folder isn't a git repository), show the error and its fix.
