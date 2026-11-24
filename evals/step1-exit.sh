#!/usr/bin/env bash
# Build step 1's exit test: headless Claude Code, inside a fresh Ahjo environment,
# builds Paja and runs its checks with one command each, using only CLAUDE.md.
# Needs `claude` signed in, so it runs on the owner's machine, not in CI.
set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
home=$(mktemp -d)
name="exit-$(date +%s)"
export PAJA_HOME="$home"
cleanup() {
  cargo run -q --manifest-path "$repo/Cargo.toml" -- --json env remove "$name" --force >/dev/null 2>&1 || true
  git -C "$repo" branch -D "paja/$name" >/dev/null 2>&1 || true
  rm -rf "$home"
}
trap cleanup EXIT

env_json=$(cargo run -q --manifest-path "$repo/Cargo.toml" -- --json env create "$name" --repo "$repo")
path=$(printf '%s' "$env_json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["path"])')

prompt='You are in a fresh checkout of Paja. Using only CLAUDE.md for guidance, build Paja and run its full checks.
Reply with only this JSON: {"commands":[{"cmd":"...","passed":true}]}'
out=$(cd "$path" && claude -p "$prompt" --allowedTools "Bash(cargo *)" "Read" --output-format json </dev/null)

printf '%s' "$out" | python3 -c '
import json, re, sys
result = json.load(sys.stdin)["result"]
report = json.loads(re.search(r"\{.*\}", result, re.S).group(0))
cmds = {c["cmd"]: c["passed"] for c in report["commands"]}
need = ["cargo fmt --all --check", "cargo clippy --all-targets -- -D warnings", "cargo test"]
missing = [c for c in need if c not in cmds]
failed = [c for c, ok in cmds.items() if not ok]
print(json.dumps({"commands": cmds, "missing": missing, "failed": failed}))
sys.exit(1 if missing or failed else 0)
'
