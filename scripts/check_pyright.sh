#!/usr/bin/env bash
# Strict Pyright check (AGENTS.md: zero errors) on all Python in tools/ and
# on the Python embedded in scripts/*.sh (`<<'PY'` heredocs and multi-line
# `python3 -c '...'` blocks). Needs `pyright` on PATH (npm i pyright); fails
# closed when it is missing.
set -euo pipefail
cd "$(dirname "$0")/.."
if ! command -v pyright >/dev/null 2>&1; then
  echo "pyright: not on PATH (npm install pyright); failing closed" >&2
  exit 1
fi
pyright -p pyrightconfig.json
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
printf '{"typeCheckingMode": "strict"}\n' >"$tmp/pyrightconfig.json"
python3 - "$tmp" <<'PY'
"""Write each Python block embedded in scripts/*.sh to its own file."""
import pathlib
import re
import sys

out = pathlib.Path(sys.argv[1])
count = 0
for path in sorted(pathlib.Path("scripts").glob("*.sh")):
    text = path.read_text()
    blocks = re.findall(r"<<'PY'\n(.*?)\nPY\n", text, re.S)
    blocks += re.findall(r"python3 -c '\n(.*?)'", text, re.S)
    for i, block in enumerate(blocks):
        (out / f"{path.stem}_{i}.py").write_text(block + "\n")
        count += 1
print(f"pyright: {count} embedded Python blocks from scripts/")
PY
pyright -p "$tmp/pyrightconfig.json"
