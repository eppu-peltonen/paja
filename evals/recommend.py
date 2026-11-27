#!/usr/bin/env python3
"""`paja init --recommend` with real Claude Code: the init agent, run with no repository to read
and only `Bash(pwd)` among its tools, still answers with structured output.

The smoke tests drive this path with a fake `claude` that prints a canned answer, so they can't
show that a real agent, under the deny-list hook (and, with the sandbox, inside it), returns an
answer of the right shape. This eval runs `paja --json init --recommend` twice: with the sandbox
(`PAJA_SBX`, the `sbx` program) and without. The verdict never rests on the model's report. It
comes from evidence:

- `paja` exits 0 and prints one JSON value;
- it holds one to three recommendations, each with a stack, a why, tradeoffs and a skeleton of
  `built-in` or `generated` (`built-in` exactly for rust, node and python);
- a stated preference that fits is honoured: `python` is among the recommendations;
- nothing is left of the agent's scratch repository under the home's `envs/`.

Needs a signed-in `claude` (and `sbx` for the sandbox case), so it runs on the owner's machine,
not in CI. Works on Windows and Unix. Claude Code's sign-in files from the real home are copied
into a temporary HOME for the run and deleted with it. Prints one JSON value, and a pass or fail
line per case on stderr; exits 0 only when every case passes.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import traceback

STEP_TIMEOUT = 15 * 60
WINDOWS = os.name == "nt"
BUILDING = "a small web app where a family keeps its reading list"
BUILT_IN = {"rust", "node", "python"}


def kill_tree(proc):
    if WINDOWS:
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)], capture_output=True)
    else:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def run(cmd, cwd=None, env=None, timeout=STEP_TIMEOUT):
    """Run a command; on timeout, kill the whole process tree and raise."""
    proc = subprocess.Popen(
        cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL,
        start_new_session=not WINDOWS,
    )
    try:
        out, err = proc.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        kill_tree(proc)
        try:
            proc.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            pass
        raise
    return subprocess.CompletedProcess(cmd, proc.returncode, out, err)


def evidence(passed, detail=""):
    return {"passed": bool(passed), "detail": detail}


def judge(done, paja_home):
    ev = {}
    try:
        out = json.loads(done.stdout)
    except ValueError:
        out = None
    ev["paja exits 0 with one JSON value"] = evidence(
        done.returncode == 0 and isinstance(out, dict) and "error" not in out,
        f"exit {done.returncode}; stdout: {done.stdout.strip()[-500:]}; "
        f"stderr: {done.stderr.strip()[-500:]}",
    )
    options = (out or {}).get("recommendations")
    shaped = isinstance(options, list) and 1 <= len(options) <= 3
    bad = []
    for option in options if isinstance(options, list) else []:
        stack = option.get("stack") if isinstance(option, dict) else None
        if not isinstance(stack, str) or not stack.strip():
            bad.append(f"no stack in {option!r}")
            continue
        for field in ("why", "tradeoffs"):
            if not isinstance(option.get(field), str) or not option[field].strip():
                bad.append(f"{stack}: no {field}")
        want = "built-in" if stack in BUILT_IN else "generated"
        if option.get("skeleton") != want:
            bad.append(f"{stack}: skeleton {option.get('skeleton')!r}, want {want!r}")
    ev["one to three well-formed recommendations"] = evidence(
        shaped and not bad, f"recommendations: {options!r}; problems: {bad}"
    )
    stacks = [o.get("stack") for o in options if isinstance(o, dict)] if shaped else []
    ev["the preferred python is offered"] = evidence("python" in stacks, f"stacks: {stacks}")
    envs = os.path.join(paja_home, "envs")
    left = sorted(os.listdir(envs)) if os.path.isdir(envs) else []
    ev["nothing is left of the scratch repository"] = evidence(not left, f"left in envs/: {left}")
    return ev


def one_case(case, paja, base_env):
    scratch = tempfile.mkdtemp(prefix="paja-eval-recommend-")
    try:
        home, paja_home = os.path.join(scratch, "home"), os.path.join(scratch, "paja-home")
        cwd = os.path.join(scratch, "work")
        for d in (home, paja_home, cwd):
            os.makedirs(d)
        # Claude Code's own sign-in, so the headless agent can run under the temporary HOME.
        real_home = os.path.expanduser("~")
        if os.path.isfile(os.path.join(real_home, ".claude.json")):
            shutil.copy(os.path.join(real_home, ".claude.json"), os.path.join(home, ".claude.json"))
        creds = os.path.join(real_home, ".claude", ".credentials.json")
        if os.path.isfile(creds):
            os.makedirs(os.path.join(home, ".claude"))
            shutil.copy(creds, os.path.join(home, ".claude", ".credentials.json"))
        env = dict(base_env)
        env.update({"HOME": home, "USERPROFILE": home, "PAJA_HOME": paja_home})
        env.pop("PAJA_SBX", None)
        if case["sandbox"]:
            env["PAJA_SBX"] = case["sandbox"]
        done = run(
            [paja, "--json", "init", "--recommend", BUILDING, "--prefer", "python"], cwd, env
        )
        ev = judge(done, paja_home)
        return {"passed": all(e["passed"] for e in ev.values()), "evidence": ev}
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    root = run(["git", "rev-parse", "--show-toplevel"], here).stdout.strip()
    claude = os.environ.get("PAJA_CLAUDE") or "claude"
    for tool in (claude, "cargo", "git"):
        if not shutil.which(tool):
            return {"error": f"`{tool}` is not on PATH; install it (and sign in to claude), then rerun"}, False
    base_env = dict(os.environ)
    built = run(["cargo", "build", "-q", "--bins"], root, base_env, timeout=30 * 60)
    if built.returncode != 0:
        return {"error": f"`cargo build` failed in {root}: {built.stderr.strip()[-500:]}"}, False
    target = os.environ.get("CARGO_TARGET_DIR") or os.path.join(root, "target")
    paja = os.path.join(target, "debug", "paja" + (".exe" if WINDOWS else ""))

    sbx = os.environ.get("PAJA_SBX") or shutil.which("sbx")
    results = {}
    for case in ({"name": "with the sandbox", "sandbox": sbx},
                 {"name": "without the sandbox", "sandbox": None}):
        if case["name"] == "with the sandbox" and not sbx:
            result = {"passed": False, "error": "`sbx` is not on PATH and PAJA_SBX is not set, so "
                      "the sandbox case can't run; install Docker Sandboxes (sbx), then rerun"}
        else:
            try:
                result = one_case(case, paja, base_env)
            except Exception as err:
                result = {"passed": False, "error": f"the case crashed: {err!r}",
                          "trace": traceback.format_exc()[-1500:]}
        results[case["name"]] = result
        print(f"{'PASS' if result['passed'] else 'FAIL'}: {case['name']}", file=sys.stderr)
        for name, ev in result.get("evidence", {}).items():
            print(f"  {'pass' if ev['passed'] else 'FAIL'}: {name}"
                  + ("" if ev["passed"] else f" -> {ev['detail']}"), file=sys.stderr)
        if "error" in result:
            print(f"  error: {result['error']}", file=sys.stderr)
    ok = all(r["passed"] for r in results.values())
    return {"passed": ok, "cases": results}, ok


if __name__ == "__main__":
    try:
        result, ok = main()
    except Exception as err:  # one JSON value, always
        result, ok = {"error": f"the eval crashed: {err!r}; rerun, and report this if it repeats",
                      "trace": traceback.format_exc()[-1500:]}, False
    print(json.dumps(result))
    sys.exit(0 if ok else 1)
