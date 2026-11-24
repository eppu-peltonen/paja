#!/usr/bin/env python3
"""Build step 1's exit test.

Headless Claude Code, inside a fresh Ahjo environment made from `main` and guided only by the
repository's CLAUDE.md, must build Paja and run its checks with one command each.

The verdict never rests on the model's own report: it is read from the transcript (each command
Claude actually ran and whether it failed), and the checks are then re-run here in the same
environment. Needs a signed-in `claude`, so it runs on the owner's machine, not in CI.

Prints one JSON value; exits 0 only when the test passes.
"""

import json
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile

# Each step and how to recognise its command, whatever flags or prefixes Claude adds.
STEPS = {
    "build": re.compile(r"\bcargo\s+build\b"),
    "fmt": re.compile(r"\bcargo\s+fmt\b.*--check"),
    "clippy": re.compile(r"\bcargo\s+clippy\b"),
    "test": re.compile(r"\bcargo\s+test\b"),
}
# How Claude Code words a tool call that the permission rules refused.
DENIED = re.compile(r"permission|requires approval|haven't granted", re.I)
RECHECK = [
    ["cargo", "fmt", "--all", "--check"],
    ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"],
    ["cargo", "test"],
]
PROMPT = (
    "You are in a fresh checkout of Paja. Using only CLAUDE.md for guidance, "
    "build Paja and run its full checks."
)


def done(result, ok):
    print(json.dumps(result))
    sys.exit(0 if ok else 1)


def run(cmd, cwd, env=None):
    return subprocess.run(
        cmd, cwd=cwd, env=env, capture_output=True, text=True, stdin=subprocess.DEVNULL
    )


def transcript_commands(stream):
    """(command, failed) for every Bash call in a stream-json transcript, in order."""
    calls, results, order = {}, {}, []
    for line in stream.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        inner = msg.get("message") if isinstance(msg, dict) else None
        content = inner.get("content") if isinstance(inner, dict) else None
        if not isinstance(content, list):
            continue
        for part in content:
            if not isinstance(part, dict):
                continue
            if part.get("type") == "tool_use" and part.get("name") == "Bash":
                calls[part["id"]] = part.get("input", {}).get("command", "")
                order.append(part["id"])
            elif part.get("type") == "tool_result":
                text = json.dumps(part.get("content"))
                denied = bool(part.get("is_error")) and DENIED.search(text) is not None
                results[part.get("tool_use_id")] = "denied" if denied else bool(part.get("is_error"))
    # A call with no result never finished: count it as failed. A call the permission rules
    # refused never ran, so it says nothing about the checks.
    return [(calls[i], results.get(i, True)) for i in order if results.get(i) != "denied"]


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    repo = run(["git", "rev-parse", "--show-toplevel"], here).stdout.strip()
    if not shutil.which("claude"):
        done({"error": "`claude` is not on PATH; install Claude Code and sign in, then rerun"}, False)
    home = tempfile.mkdtemp(prefix="paja-eval-")
    name = f"exit-{secrets.token_hex(4)}"
    env = dict(os.environ, PAJA_HOME=home)
    paja = ["cargo", "run", "-q", "--manifest-path", os.path.join(repo, "Cargo.toml"), "--", "--json"]
    try:
        created = run(paja + ["env", "create", name, "--repo", repo, "--base", "main"], repo, env)
        try:
            info = json.loads(created.stdout)
        except json.JSONDecodeError:
            info = {"error": created.stderr.strip()}
        if "path" not in info:
            done({"error": f"`paja env create` failed: {info.get('error')}"}, False)
        path = info["path"]

        # Project settings only: the owner's own CLAUDE.md and settings must not help.
        claude = run(
            ["claude", "-p", PROMPT, "--setting-sources", "project",
             "--allowedTools", "Bash(cargo *)", "Read",
             "--output-format", "stream-json", "--verbose"],
            path,
        )
        if claude.returncode != 0:
            done({"error": f"claude exited {claude.returncode}: {claude.stderr.strip()[-500:]}; "
                           "check that `claude` is signed in, then rerun"}, False)

        commands = transcript_commands(claude.stdout)
        steps = {}
        for step, pattern in STEPS.items():
            runs = [failed for cmd, failed in commands if pattern.search(cmd)]
            # "One command each": the step was run, and its first attempt passed.
            steps[step] = {"runs": len(runs), "first_passed": bool(runs) and not runs[0]}
        recheck = {" ".join(c): run(c, path).returncode == 0 for c in RECHECK}

        ok = all(s["first_passed"] for s in steps.values()) and all(recheck.values())
        done({"passed": ok, "steps": steps, "recheck": recheck,
              "commands": [c for c, _ in commands]}, ok)
    finally:
        run(paja + ["env", "remove", name, "--force"], repo, env)
        shutil.rmtree(home, ignore_errors=True)
        run(["git", "worktree", "prune"], repo)
        run(["git", "branch", "-D", f"paja/{name}"], repo)


if __name__ == "__main__":
    main()
