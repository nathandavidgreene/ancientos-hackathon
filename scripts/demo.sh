#!/usr/bin/env bash
# End-to-end offline demo: 14 storyboard shots on synthetic fixtures.
# Mock/local clients only; no network, no paid calls, no real data.
set -euo pipefail
cd "$(dirname "$0")/.."
unset ANCIENTOS_ALLOW_PAID
unset META_API_KEY MODAL_LLAMA_URL ANCIENTOS_LOCAL_LLM_URL ANCIENTOS_LOCAL_LLM_MODEL X_CLIENT_ID || true
# Semantic ranking stays opt-in: never on in the demo.
unset ANCIENTOS_EMBED_MODEL ANCIENTOS_SEMANTIC_WEIGHT || true
DATA="$(mktemp -d)"
trap 'rm -rf "$DATA"' EXIT
# Print usage and exit.
usage() {
  echo "usage: scripts/demo.sh [--shot N]" >&2
  echo "  N is 1..14; no args runs all shots" >&2
}
if [ $# -eq 1 ] || [ $# -gt 2 ]; then
  usage
  exit 2
fi
if [ $# -eq 2 ]; then
  if [ "$1" != "--shot" ]; then
    usage
    exit 2
  fi
  case "$2" in
    1|2|3|4|5|6|7|8|9|10|11|12|13|14) ;;
    *) echo "error: invalid shot '$2'" >&2; usage; exit 2 ;;
  esac
fi
export CARGO_HOME="${CARGO_HOME:-/tmp/cargo-home}"
cargo build -q -p demo-runner -p bookmarks-agent
# The demo swaps HOME to a temp dir; rustup still needs the real one.
REAL_HOME="$HOME"
export ANCIENTOS_HOME="$DATA/home"
export HOME="$DATA/home"
export DECISION_LOG_PATH="$DATA/decision_log.jsonl"
export IDEA_BACKLOG_PATH="$DATA/backlog.md"
export ANCIENTOS_FIXTURE=""
export ANCIENTOS_DEMO_FIXED_NOW="2026-10-26T00:00:00Z"
mkdir -p "$ANCIENTOS_HOME"
# Run the demo-runner binary against the demo data dir.
run() { ./target/debug/demo-runner --data "$DATA" "$@"; }

FIX1="fixtures/demo/synthetic_bookmarks.jsonl"
FIX2="fixtures/demo/try_it_and_thread_bookmarks.jsonl"
FIX3="fixtures/demo/deeper_thread_bookmarks.jsonl"

# Shot 1: print the "Saved != read" beat.
shot1() {
  echo "==== Shot 1/14: Saved != read ===="
  run count "$FIX1" "$FIX2"
}

# Shot 2: print the "Rust rebuild" beat.
shot2() {
  echo "==== Shot 2/14: Rust rebuild ===="
  echo "WordCraft (public post, handle blurred) -> repo tree:"
  echo "crates/core"
  echo "crates/router"
  echo "crates/bookmarks-agent"
  echo "crates/muse-runs"
  echo "crates/web-ui"
  echo "crates/eval"
  echo "crates/demo-runner"
}

# Shot 3: print the "Governed agent core" beat.
shot3() {
  echo "==== Shot 3/14: Governed agent core ===="
  echo "scheduler -> gate -> decision_log"
  echo "privacy router: LocalLlama (local, sensitive) / MuseSpark (sanitized)"
  echo "fail-closed gate; append-only, hash-chained decision log"
}

# Shot 4: print the "Read-only sync" beat.
shot4() {
  echo "==== Shot 4/14: Read-only sync ===="
  run sync "$FIX1"
  run sync "$FIX2"
}

# Shot 5: print the "Morning brief" beat.
shot5() {
  echo "==== Shot 5/14: Morning brief ===="
  run brief-rich 3
  echo "Translate (mock, original kept):"
  run translate-demo
}

# Shot 6: print the "Privacy routing" beat.
shot6() {
  echo "==== Shot 6/14: Privacy routing ===="
  run routing
}

# Shot 7: print the "Voting and thresholds" beat.
shot7() {
  echo "==== Shot 7/14: Voting and thresholds ===="
  run vote-demo
  echo "Never this author (hidden, then undone):"
  run author-demo
}

# Shot 8: print the "Delegate to idea" beat.
shot8() {
  echo "==== Shot 8/14: Delegate to idea ===="
  run delegate-demo demo-2001
}

# Shot 9: print the "Try it" beat.
shot9() {
  echo "==== Shot 9/14: Try it ===="
  run try-demo
}

# Shot 10: print the "Provable log" beat.
shot10() {
  echo "==== Shot 10/14: Provable log ===="
  run log
  run tamper-demo
}

# Shot 11: print the "Agent proposes, you approve" beat.
shot11() {
  echo "==== Shot 11/14: Agent proposes, you approve ===="
  echo "Paid is simulated via a mock (no network, ANCIENTOS_ALLOW_PAID unset):"
  run approvals-propose
  echo "bookmarks-agent approvals:"
  ./target/debug/bookmarks-agent approvals
  APPROVAL_ID=$(./target/debug/bookmarks-agent approvals | awk '{print $1}')
  echo "bookmarks-agent approve $APPROVAL_ID --now 2030-01-01T00:00:01Z:"
  ./target/debug/bookmarks-agent approve "$APPROVAL_ID" --now "2030-01-01T00:00:01Z"
  echo "Next tick releases exactly once (mock, no network):"
  run approvals-release
  echo "Approval chain (requested -> approved -> released):"
  run approvals-chain
  echo "Second approve fails closed:"
  ./target/debug/bookmarks-agent approve "$APPROVAL_ID" --now "2030-01-01T00:00:02Z" 2>&1 || true
}

# Shot 12: print the "Try it in stages, measured" beat.
shot12() {
  echo "==== Shot 12/14: Try it in stages, measured ===="
  run cascade-demo
  # web-ui's /accuracy page, rendered offline from the synthetic eval set.
  run accuracy-demo
}

# Shot 13: print the "Built with Meta, gated by the core" beat.
shot13() {
  echo "==== Shot 13/14: Built with Meta, gated by the core ===="
  RUNS=$(grep -E '^\| [0-9]+[a-z]? ' docs/muse-code-log.md | grep -vc 'Not used')
  echo "Muse Code runs: $RUNS (from docs/muse-code-log.md)"
  echo "Muse Spark writes the briefs; tokens per brief: ~1.6k"
  cat docs/muse-code-log.md
  # Second app: a Muse Code run is a gated action (stub script, paid unset).
  run muse-runs-demo
}

# Shot 14: print the "Roadmap" beat.
shot14() {
  echo "==== Shot 14/14: Roadmap ===="
  echo "Roadmap: more agents on core, custom X feed ranking, go-deeper sessions."
  echo "Bookmarks and muse-runs are the first two apps; the rest of AncientOS plugs into the same core."
  # Core-plus-apps (P7): the same check CI runs; deterministic, offline.
  HOME="$REAL_HOME" scripts/check_core_boundary.sh
  echo "Go deeper (offline, quoted + linked, shortened):"
  # "Saved:" carries the temp dir path, so filter it to stay byte-identical.
  # Shortened to the first lines to keep the 3:00 budget.
  ./target/debug/bookmarks-agent deeper demo-3001 --offline "$FIX3" | grep -v '^Saved: ' | head -n 8
}

# Prepare synced fixture data without printing anything.
setup_sync_silent() {
  run sync "$FIX1" >/dev/null 2>&1
  run sync "$FIX2" >/dev/null 2>&1
}

if [ $# -eq 0 ]; then
  shot1
  shot2
  shot3
  shot4
  shot5
  shot6
  shot7
  shot8
  shot9
  shot10
  shot11
  shot12
  shot13
  shot14
elif [ $# -eq 2 ] && [ "$1" = "--shot" ]; then
  case "$2" in
    1) shot1 ;;
    2) shot2 ;;
    3) shot3 ;;
    4) shot4 ;;
    5) setup_sync_silent; shot5 ;;
    6) shot6 ;;
    7) setup_sync_silent; shot7 ;;
    8) setup_sync_silent; shot8 ;;
    9) setup_sync_silent; shot9 ;;
    10) setup_sync_silent; run brief 0 >/dev/null 2>&1; shot10 ;;
    11) shot11 ;;
    12) shot12 ;;
    13) shot13 ;;
    14) shot14 ;;
    *) echo "error: invalid shot '$2'" >&2; usage; exit 2 ;;
  esac
else
  usage
  exit 2
fi
