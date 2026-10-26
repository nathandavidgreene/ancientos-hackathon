#!/usr/bin/env bash
# Query the local Graphify index for this repository.
# Usage: scripts/graphify_query.sh "<question>"
# Overrides: GRAPHIFY_BIN (graphify executable), GRAPHIFY_OUT (index directory).
set -euo pipefail

if [ "$#" -lt 1 ] || [ -z "$1" ]; then
  echo "usage: $0 \"<question>\"" >&2
  exit 2
fi

graphify_bin="${GRAPHIFY_BIN:-$HOME/.local/venvs/graphify/bin/graphify}"
graphify_out="${GRAPHIFY_OUT:-$HOME/.cache/graphify/ancientos-hackathon}"
graph="$graphify_out/graphify-out/graph.json"

if [ ! -x "$graphify_bin" ]; then
  echo "graphify not installed at $graphify_bin (set GRAPHIFY_BIN to override)." >&2
  exit 1
fi
if [ ! -f "$graph" ]; then
  echo "No Graphify index at $graph; run scripts/graphify_refresh.sh first." >&2
  exit 1
fi

GRAPHIFY_BACKEND=none "$graphify_bin" query "$*" --graph "$graph"
