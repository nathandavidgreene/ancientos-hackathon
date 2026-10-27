#!/usr/bin/env bash
# Doc-comment coverage check: every Rust fn (except #[test] fns) needs a
# `///` doc comment, every Python module and function under tools/ needs a
# docstring, and every shell script and shell function needs a header
# comment. Prints coverage; exits 1 if anything is undocumented.
# Usage: scripts/check_docs.sh [--list]   (--list prints every gap)
set -euo pipefail
cd "$(dirname "$0")/.."
LIST="${1:-}"
python3 - "$LIST" <<'PY'
"""Scan Rust and shell sources and report undocumented functions."""
import ast
import pathlib
import re
import sys

FN = re.compile(r'^\s*(pub(\([^)]*\))?\s+)?((const|async|unsafe|extern\s+"[^"]*")\s+)*fn\s+\w+')
SH_FN = re.compile(r'^\s*(function\s+)?[A-Za-z_][A-Za-z0-9_]*\s*\(\)\s*\{')
TEST_ATTR = re.compile(r'^#\[(tokio::)?test\b')

total = 0
missing: list[str] = []
for path in sorted(pathlib.Path("crates").rglob("*.rs")):
    if "target" in path.parts:
        continue
    lines = path.read_text().splitlines()
    for i, line in enumerate(lines):
        if not FN.match(line):
            continue
        j = i - 1
        attrs: list[str] = []
        while j >= 0:
            s = lines[j].strip()
            if s.startswith("#["):
                attrs.append(s)
                j -= 1
            elif s.startswith("//") and not s.startswith(("///", "//!")):
                # Plain comment between the doc comment and the fn.
                j -= 1
            elif s.endswith("]") and not s.startswith("//"):
                # Tail of a multi-line attribute: walk back to its `#[`.
                while j >= 0 and not lines[j].strip().startswith("#["):
                    j -= 1
                if j >= 0:
                    attrs.append(lines[j].strip())
                j -= 1
            else:
                break
        if any(TEST_ATTR.match(a) for a in attrs):
            continue
        total += 1
        prev = lines[j].strip() if j >= 0 else ""
        if not (prev.startswith("///") or prev.endswith("*/")):
            missing.append(f"{path}:{i + 1}: {line.strip()[:90]}")

for path in sorted(pathlib.Path("tools").glob("*.py")):
    tree = ast.parse(path.read_text())
    total += 1
    if ast.get_docstring(tree) is None:
        missing.append(f"{path}:1: module has no docstring")
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            total += 1
            if ast.get_docstring(node) is None:
                missing.append(f"{path}:{node.lineno}: def {node.name}")

for path in sorted([*pathlib.Path("scripts").glob("*.sh"), *pathlib.Path("tools").glob("*.sh")]):
    lines = path.read_text().splitlines()
    total += 1
    if len(lines) < 2 or not lines[1].startswith("# "):
        missing.append(f"{path}:2: script has no header comment")
    for i, line in enumerate(lines):
        if SH_FN.match(line):
            total += 1
            if i == 0 or not lines[i - 1].strip().startswith("#"):
                missing.append(f"{path}:{i + 1}: {line.strip()[:90]}")

documented = total - len(missing)
pct = 100.0 * documented / total if total else 100.0
if sys.argv[1] == "--list":
    print("\n".join(missing))
print(f"docs: {documented}/{total} functions documented ({pct:.1f}%)")
if missing:
    print(f"docs: {len(missing)} undocumented (run scripts/check_docs.sh --list)", file=sys.stderr)
    sys.exit(1)
PY
