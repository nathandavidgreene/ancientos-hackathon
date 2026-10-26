#!/usr/bin/env bash
# Rebuild the local code-only Graphify index for this repository.
# Overrides: GRAPHIFY_BIN (graphify executable), GRAPHIFY_OUT (index directory).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
graphify_bin="${GRAPHIFY_BIN:-$HOME/.local/venvs/graphify/bin/graphify}"
graphify_out="${GRAPHIFY_OUT:-$HOME/.cache/graphify/ancientos-hackathon}"

if [ ! -x "$graphify_bin" ]; then
  echo "graphify not installed at $graphify_bin (set GRAPHIFY_BIN to override); skipping refresh." >&2
  exit 0
fi

cd "$repo_root"
mkdir -p "$graphify_out"
GRAPHIFY_BACKEND=none "$graphify_bin" extract . --code-only --out "$graphify_out"
GRAPHIFY_BACKEND=none "$graphify_bin" cluster-only "$graphify_out" --no-label
echo "Graphify index refreshed: $graphify_out/graphify-out/graph.json"
