#!/usr/bin/env python3
"""`paja init --recommend` with real Claude Code: the init agent, run with no repository to read
and only `Bash(pwd)` among its tools, still answers with structured output.

The smoke tests drive this path with a fake `claude` that prints a canned answer, so they can't
show that a real agent, under the deny-list hook (and, with the sandbox, inside it), returns an
answer of the right shape. This eval runs `paja --json init --recommend` twice: with the sandbox
(`PAJA_SBX`, the `sbx` program, with a sandbox of the eval's own, `paja-eval-recommend`; the
person's `paja` sandbox is never touched) and without. The verdict never rests on the model's
report. It comes from evidence:

- `paja` exits 0 and prints one JSON value;
- it holds one to three recommendations, each with a stack, a why, tradeoffs and a skeleton of
  `built-in` or `generated` (`built-in` exactly for rust, node and python);
- a stated preference that fits is honoured: `python` is among the recommendations (a failure
  of this alone is the model's choice, not the harness);
- `paja` says on stderr that it is starting the sandbox (or that there is none);
- nothing is left of the agent's scratch repository under the home's `envs/`.

Needs a signed-in `claude` (and `sbx` for the sandbox case), so it runs on the owner's machine,
not in CI. Works on Windows and Unix. It works in a folder of its own, the same on every run
(under `~/.cache/paja-evals/`, or `%LOCALAPPDATA%\\paja-evals\\` on Windows), private to the
person and emptied before and after each case; Claude Code's sign-in files from the real home are
copied into a HOME there for the run and deleted with it. The sandbox case uses a sandbox of its
own, the same on every run, that the person signs Claude Code in to once. Prints one JSON value,
and a pass or fail line per case on stderr; exits 0 only when every case passes. Two runs of one
eval at once share its folder, so run one at a time.
"""

import json
import os
import shutil
import signal
import stat
import subprocess
import sys
import traceback

STEP_TIMEOUT = 15 * 60
WINDOWS = os.name == "nt"
BUILDING = "a small web app where a family keeps its reading list"
BUILT_IN = {"rust", "node", "python"}
# The eval's own sandbox and folder, the same every run: the person signs Claude Code in to the
# sandbox once, and its mounts and toolchain stay valid. The person's `paja` sandbox is never used.
SANDBOX = "paja-eval-recommend"


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


def eval_root(name):
    """The eval's own folder, the same on every run and private to the person: under their local
    application data on Windows, else their cache directory. Refused unless it and its parent are
    real directories (not links) that the person owns and nobody else can write to."""
    if WINDOWS:
        base = os.environ.get("LOCALAPPDATA") or os.path.join(
            os.path.expanduser("~"), "AppData", "Local")
    else:
        base = os.environ.get("XDG_CACHE_HOME") or os.path.join(os.path.expanduser("~"), ".cache")
    root = os.path.join(base, "paja-evals", name)
    os.makedirs(root, mode=0o700, exist_ok=True)
    for path in (os.path.dirname(root), root):
        st = os.lstat(path)
        if not stat.S_ISDIR(st.st_mode) or _is_link(path, st):
            raise RuntimeError(f"{path} is not a plain directory; remove it, then rerun")
        if not WINDOWS:
            if st.st_uid != os.getuid():
                raise RuntimeError(f"{path} belongs to another user; remove it, then rerun")
            if st.st_mode & 0o077:
                os.chmod(path, 0o700)
    return root


def _is_link(path, st):
    """A symbolic link, or on Windows any reparse point (a junction too)."""
    return stat.S_ISLNK(st.st_mode) or bool(getattr(st, "st_file_attributes", 0) & 0x400)


def _retry(func, path, _exc):
    """`shutil.rmtree`'s error handler: git writes read-only object files, which Windows won't
    delete until they are writable. Anything else is raised."""
    if WINDOWS and func in (os.unlink, os.remove, os.rmdir):
        os.chmod(path, stat.S_IWRITE)
        func(path)
    else:
        raise


def _remove(path):
    """Remove one entry, never following a link: code in the sandbox can write in the mounted
    folders, and a link there must not lead the cleanup to the person's own files. A directory
    goes with `shutil.rmtree`, which on Unix walks by file descriptor and so never follows a link
    swapped in while it runs; on Windows it doesn't enter links or junctions either."""
    st = os.lstat(path)
    if _is_link(path, st):
        try:
            os.unlink(path)
        except OSError:
            os.rmdir(path)  # a Windows link or junction to a directory
    elif stat.S_ISDIR(st.st_mode):
        if sys.version_info >= (3, 12):
            shutil.rmtree(path, onexc=_retry)
        else:
            shutil.rmtree(path, onerror=_retry)
    else:
        try:
            os.unlink(path)
        except PermissionError:
            if not WINDOWS:
                raise
            os.chmod(path, stat.S_IWRITE)
            os.unlink(path)


def reset(root, keep, first=()):
    """Empty `root` but keep the directories in `keep` (emptied) and those on the way to them.
    The eval's sandbox mounts those, so they stay where the sandbox made on the first run sees
    them; a directory deleted and made again at the same path may not be visible in it. The
    paths in `first` go before anything else (the copied sign-in), and one entry that can't be
    removed doesn't stop the rest: the errors are raised together at the end."""
    kept = [os.path.normcase(os.path.abspath(k)) for k in keep]
    errors = []

    def attempt(p):
        try:
            _remove(p)
        except OSError as err:
            errors.append(f"{p}: {err}")

    for p in first:
        if os.path.lexists(p):
            attempt(p)

    def clear(d):
        for entry in os.listdir(d):
            p = os.path.join(d, entry)
            n = os.path.normcase(os.path.abspath(p))
            st = os.lstat(p)
            on_the_way = any(k == n or k.startswith(n + os.sep) for k in kept)
            if on_the_way and stat.S_ISDIR(st.st_mode) and not _is_link(p, st):
                clear(p)
            else:
                attempt(p)

    clear(root)
    for k in keep:
        os.makedirs(k, exist_ok=True)
    if errors:
        raise OSError("couldn't remove " + "; ".join(errors))


def evidence(passed, detail=""):
    return {"passed": bool(passed), "detail": detail}


def judge(done, paja_home, sandboxed):
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
        # Paja decides the skeleton by the stack's lowercase name.
        want = "built-in" if stack.strip().lower() in BUILT_IN else "generated"
        if option.get("skeleton") != want:
            bad.append(f"{stack}: skeleton {option.get('skeleton')!r}, want {want!r}")
    ev["one to three well-formed recommendations"] = evidence(
        shaped and not bad, f"recommendations: {options!r}; problems: {bad}"
    )
    stacks = [str(o.get("stack", "")).strip().lower() for o in options if isinstance(o, dict)] \
        if shaped else []
    ev["the preferred python is offered"] = evidence(
        any("python" in stack for stack in stacks), f"stacks: {stacks}"
    )
    envs = os.path.join(paja_home, "envs")
    left = sorted(os.listdir(envs)) if os.path.isdir(envs) else []
    # Paja says on stderr which way the agent ran; with PAJA_SBX set it fails closed rather than
    # running without the sandbox, and this pins that it took the sandbox path at all.
    said = "starting the sandbox" if sandboxed else "no sandbox"
    ev[f"paja says {said!r}"] = evidence(
        said in done.stderr, f"stderr: {done.stderr.strip()[-500:]}"
    )
    ev["nothing is left of the scratch repository"] = evidence(not left, f"left in envs/: {left}")
    return ev


def one_case(case, paja, base_env):
    scratch = eval_root("recommend")
    home, paja_home = os.path.join(scratch, "home"), os.path.join(scratch, "paja-home")
    keep = [os.path.join(paja_home, "envs")]
    reset(scratch, keep, first=[home])
    try:
        cwd = os.path.join(scratch, "work")
        for d in (home, cwd):
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
        for var in ("PAJA_SBX", "PAJA_SBX_NAME"):
            env.pop(var, None)
        if case["sandbox"]:
            env["PAJA_SBX"] = case["sandbox"]
            env["PAJA_SBX_NAME"] = SANDBOX
        done = run(
            [paja, "--json", "init", "--recommend", BUILDING, "--prefer", "python"], cwd, env
        )
        if case["sandbox"] and "not signed in" in done.stdout:
            return {"passed": False, "error": f"Claude Code in the eval's sandbox is not signed in; "
                    f"sign in once with `{case['sandbox']} exec -it {SANDBOX} claude` and "
                    "`/login`, then rerun"}
        ev = judge(done, paja_home, bool(case["sandbox"]))
        return {"passed": all(e["passed"] for e in ev.values()), "evidence": ev}
    finally:
        try:
            reset(scratch, keep, first=[home])
        except OSError as err:
            print(f"warning: delete {home}; it holds a copy of Claude Code's sign-in ({err})",
                  file=sys.stderr)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    root = run(["git", "rev-parse", "--show-toplevel"], here).stdout.strip()
    # A PAJA_CLAUDE for the sandbox names a program there, which the host may not have.
    tools = ["cargo", "git"] + ([] if os.environ.get("PAJA_CLAUDE") else ["claude"])
    for tool in tools:
        if not shutil.which(tool):
            return {"error": f"`{tool}` is not on PATH; install it (and sign in to claude), "
                    "then rerun"}, False
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
