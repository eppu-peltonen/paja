#!/usr/bin/env python3
"""Build step 1's exit test.

Headless Claude Code, inside a fresh Ahjo environment made from `origin/main` and guided only
by the repository's CLAUDE.md, must build Paja and run its checks with one command each.

The verdict never rests on the model's own report. A `cargo` shim placed first on PATH logs
every cargo process Claude starts, with its arguments and exit code, so each check is judged
by what really ran. The checks are then re-run here in the same environment. Claude and the
re-runs get a throwaway PAJA_HOME, so the owner's real state is never touched.

Needs a signed-in `claude`, so it runs on the owner's machine, not in CI.
Prints one JSON value; exits 0 only when the test passes.
"""

import json
import os
import signal
import shutil
import subprocess
import sys
import tempfile
import traceback

CLAUDE_TIMEOUT = 20 * 60
CHECK_TIMEOUT = 10 * 60
# CLAUDE.md's checks, recognised by cargo subcommand.
CHECKS = ["fmt", "clippy", "test"]
RECHECK = [
    ["cargo", "fmt", "--all", "--check"],
    ["cargo", "clippy", "--all-targets", "--", "-D", "warnings"],
    ["cargo", "test"],
]
PROMPT = (
    "You are in a fresh checkout of Paja. Using only CLAUDE.md for guidance, "
    "build Paja and run its full checks."
)
SHIM = """#!/bin/sh
"$PAJA_EVAL_REAL_CARGO" "$@"
code=$?
python3 -c 'import json,sys; print(json.dumps({"args": sys.argv[2:], "code": int(sys.argv[1])}))' \\
  "$code" "$@" >> "$PAJA_EVAL_CARGO_LOG"
exit $code
"""


def done(result, ok):
    print(json.dumps(result))
    sys.exit(0 if ok else 1)


def run(cmd, cwd, env=None, timeout=CHECK_TIMEOUT):
    """Run a command in its own process group; on timeout, kill the whole group."""
    proc = subprocess.Popen(
        cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        stdin=subprocess.DEVNULL, start_new_session=True,
    )
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.communicate()
        raise
    return subprocess.CompletedProcess(cmd, proc.returncode, out, err)


ALIASES = {"t": "test"}


def is_check(args, check):
    """Whether a cargo argument list runs `check`, wherever global flags put it."""
    before = args[: args.index("--")] if "--" in args else args
    return any(ALIASES.get(a, a) == check for a in before)


def rewrites(args):
    """Whether a cargo run edits source instead of checking it."""
    before = args[: args.index("--")] if "--" in args else args
    if "fix" in before or "--fix" in before:
        return True
    return is_check(args, "fmt") and "--check" not in before


def judge(invocations):
    steps = {}
    for check in CHECKS:
        runs = [i for i in invocations if is_check(i["args"], check)]
        first = runs[0] if runs else None
        rewrote = any(rewrites(i["args"]) for i in runs)
        steps[check] = {
            "runs": len(runs),
            "first_passed": first is not None and first["code"] == 0 and not rewrote,
            "rewrote_files": rewrote,
        }
    return steps


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    repo = run(["git", "rev-parse", "--show-toplevel"], here).stdout.strip()
    for tool in ("claude", "cargo", "git"):
        if not shutil.which(tool):
            done({"error": f"`{tool}` is not on PATH; install it (and sign in to claude), then rerun"}, False)
    fetched = run(["git", "fetch", "-q", "origin", "main"], repo)
    if fetched.returncode != 0:
        done({"error": f"`git fetch origin main` failed in {repo}: {fetched.stderr.strip()}"}, False)
    base = run(["git", "rev-parse", "origin/main"], repo).stdout.strip()

    scratch = tempfile.mkdtemp(prefix="paja-eval-")
    home, shim_dir = os.path.join(scratch, "home"), os.path.join(scratch, "bin")
    os.makedirs(shim_dir)
    log = os.path.join(scratch, "cargo.jsonl")
    shim = os.path.join(shim_dir, "cargo")
    with open(shim, "w") as f:
        f.write(SHIM)
    os.chmod(shim, 0o755)
    name = f"exit-{os.urandom(4).hex()}"
    env = dict(os.environ, PAJA_HOME=home)
    # The paja that makes the environment is built from the same origin/main, not from
    # whatever the owner has checked out.
    src = os.path.join(scratch, "src")
    paja = [os.path.join(scratch, "target", "debug", "paja"), "--json"]
    try:
        added = run(["git", "worktree", "add", "-q", "--detach", src, base], repo)
        built = run(["cargo", "build", "-q", "--target-dir", os.path.join(scratch, "target")], src,
                    env) if added.returncode == 0 else added
        if built.returncode != 0:
            done({"error": f"building paja from {base} failed: {built.stderr.strip()[-500:]}"}, False)
        created = run(paja + ["env", "create", name, "--repo", repo, "--base", base], repo, env)
        try:
            info = json.loads(created.stdout)
        except json.JSONDecodeError:
            info = {"error": created.stderr.strip()}
        if "path" not in info:
            done({"error": f"`paja env create` failed: {info.get('error')}"}, False)
        path = info["path"]

        claude_env = dict(
            env,
            PATH=shim_dir + os.pathsep + env.get("PATH", ""),
            PAJA_EVAL_REAL_CARGO=shutil.which("cargo"),
            PAJA_EVAL_CARGO_LOG=log,
        )
        # Project settings only: the owner's own CLAUDE.md and settings must not help.
        try:
            claude = run(
                ["claude", "-p", PROMPT, "--setting-sources", "project",
                 "--allowedTools", "Bash(cargo *)", "Read", "--output-format", "json"],
                path, claude_env, timeout=CLAUDE_TIMEOUT,
            )
        except subprocess.TimeoutExpired:
            done({"error": f"claude did not finish within {CLAUDE_TIMEOUT}s; rerun, and if it "
                           "keeps stalling, look at what it does with `claude -p` in a fresh env"}, False)
        if claude.returncode != 0:
            done({"error": f"claude exited {claude.returncode}: {claude.stderr.strip()[-500:]}; "
                           "check that `claude` is signed in, then rerun"}, False)

        invocations = []
        if os.path.exists(log):
            with open(log) as f:
                for line in f:
                    try:
                        invocations.append(json.loads(line))
                    except json.JSONDecodeError:
                        invocations.append({"args": ["<unreadable log line>"], "code": -1})
        steps = judge(invocations)
        # The checks must pass on origin/main as it is, so Claude must leave no edits behind.
        changed = run(["git", "status", "--porcelain"], path).stdout.strip()
        recheck = {" ".join(c): run(c, path, env).returncode == 0 for c in RECHECK}
        ok = (all(s["first_passed"] for s in steps.values()) and all(recheck.values())
              and not changed)
        try:
            said = json.loads(claude.stdout).get("result", "")
        except (json.JSONDecodeError, AttributeError):
            said = claude.stdout
        done({"passed": ok, "base": base, "steps": steps, "recheck": recheck,
              "changed_files": changed.splitlines(),
              "cargo": [" ".join(i["args"]) + f" -> {i['code']}" for i in invocations],
              "claude_said": said[-1500:]}, ok)
    finally:
        try:
            run(paja + ["env", "remove", name, "--force"], repo, env)
            run(["git", "worktree", "remove", "--force", src], repo)
            run(["git", "worktree", "prune"], repo)
            run(["git", "branch", "-D", f"paja/{name}"], repo)
        finally:
            shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as err:  # one JSON value, always
        done({"error": f"the eval crashed: {err!r}; rerun, and report this if it repeats",
              "trace": traceback.format_exc()[-1500:]}, False)
