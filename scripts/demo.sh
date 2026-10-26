#!/usr/bin/env bash
# End-to-end offline demo: 12 storyboard shots on synthetic fixtures.
# Mock/local clients only; no network, no paid calls, no real data.
set -euo pipefail
cd "$(dirname "$0")/.."
unset ANCIENTOS_ALLOW_PAID
unset META_API_KEY MODAL_LLAMA_URL X_CLIENT_ID || true
DATA="$(mktemp -d)"
trap 'rm -rf "$DATA"' EXIT
usage() {
  echo "usage: scripts/demo.sh [--shot N]" >&2
  echo "  N is 1..12; no args runs all shots" >&2
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
    1|2|3|4|5|6|7|8|9|10|11|12) ;;
    *) echo "error: invalid shot '$2'" >&2; usage; exit 2 ;;
  esac
fi
export CARGO_HOME="${CARGO_HOME:-/tmp/cargo-home}"
cargo build -q -p demo-runner -p bookmarks-agent
export ANCIENTOS_HOME="$DATA/home"
export HOME="$DATA/home"
export DECISION_LOG_PATH="$DATA/decision_log.jsonl"
export IDEA_BACKLOG_PATH="$DATA/backlog.md"
export ANCIENTOS_FIXTURE=""
export ANCIENTOS_DEMO_FIXED_NOW="2026-10-26T00:00:00Z"
mkdir -p "$ANCIENTOS_HOME"
run() { ./target/debug/demo-runner --data "$DATA" "$@"; }

FIX1="fixtures/demo/synthetic_bookmarks.jsonl"
FIX2="fixtures/demo/try_it_and_thread_bookmarks.jsonl"
FIX3="fixtures/demo/deeper_thread_bookmarks.jsonl"

shot1() {
  echo "==== Shot 1/12: Saved != read ===="
  run count "$FIX1" "$FIX2"
}

shot2() {
  echo "==== Shot 2/12: Rust rebuild ===="
  echo "WordCraft (public post, handle blurred) -> repo tree:"
  echo "crates/core"
  echo "crates/router"
  echo "crates/bookmarks-agent"
  echo "crates/demo-runner"
}

shot3() {
  echo "==== Shot 3/12: Governed agent core ===="
  echo "scheduler -> gate -> decision_log"
  echo "privacy router: LocalLlama (local, sensitive) / MuseSpark (sanitized)"
  echo "fail-closed gate; append-only, hash-chained decision log"
}

shot4() {
  echo "==== Shot 4/12: Read-only sync ===="
  run sync "$FIX1"
  run sync "$FIX2"
}

shot5() {
  echo "==== Shot 5/12: Morning brief ===="
  run brief-rich 3
}

shot6() {
  echo "==== Shot 6/12: Privacy routing ===="
  run routing
}

shot7() {
  echo "==== Shot 7/12: Voting and thresholds ===="
  run vote-demo
}

shot8() {
  echo "==== Shot 8/12: Delegate to idea ===="
  run delegate-demo demo-2001
}

shot9() {
  echo "==== Shot 9/12: Try it ===="
  run try-demo
}

shot10() {
  echo "==== Shot 10/12: Provable log ===="
  run log
  run tamper-demo
}

shot11() {
  echo "==== Shot 11/12: Built with Meta ===="
  RUNS=$(grep -E '^\| [0-9]+[a-z]? ' docs/muse-code-log.md | grep -vc 'Not used')
  echo "Muse Code runs: $RUNS (from docs/muse-code-log.md)"
  echo "Muse Spark writes the briefs; tokens per brief: ~1.6k"
  cat docs/muse-code-log.md
}

shot12() {
  echo "==== Shot 12/12: Roadmap ===="
  echo "Roadmap: more agents on core, custom X feed ranking, go-deeper sessions."
  echo "Bookmarks is the first agent; the rest of AncientOS plugs into the same core."
  echo "Go deeper (offline, quoted + linked):"
  # "Saved:" carries the temp dir path, so filter it to stay byte-identical.
  ./target/debug/bookmarks-agent deeper demo-3001 --offline "$FIX3" | grep -v '^Saved: '
}

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
    *) echo "error: invalid shot '$2'" >&2; usage; exit 2 ;;
  esac
else
  usage
  exit 2
fi
