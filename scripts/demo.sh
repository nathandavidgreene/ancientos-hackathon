#!/usr/bin/env bash
# End-to-end offline demo: sync -> brief -> feedback -> threshold shift -> decision log.
# Synthetic fixture data only; mock model clients; no network, no paid calls.
set -euo pipefail
cd "$(dirname "$0")/.."
unset ANCIENTOS_ALLOW_PAID
DATA="$(mktemp -d)"
trap 'rm -rf "$DATA"' EXIT
cargo build -q -p demo-runner
run() { ./target/debug/demo-runner --data "$DATA" "$@"; }

echo "== 1. Sync (synthetic fixture) =="
run sync fixtures/demo/synthetic_bookmarks.jsonl
run thresholds
for day in 0 1 2; do
  echo; echo "== 2. Brief, day $day (sensitive categories route to LocalLlama) =="
  run brief "$day"
  echo "-- feedback: approve Rust & Systems, skip the rest, never Hot Takes on day 2"
  while read -r id cat; do
    case "$cat" in
      Rust) v=approve ;;
      Hot) if [ "$day" = 2 ]; then v=never; else v=skip; fi ;;
      *) v=skip ;;
    esac
    run feedback "$id" "$v"
  done < <(jq -r '.[] | "\(.pick_id) \(.category | split(" ")[0])"' "$DATA/brief.json")
done
echo; echo "== 3. Threshold after feedback =="
run thresholds
echo; echo "== 4. Next brief: learned category is auto-approved =="
run brief 3
echo; echo "== 5. Decision log (append-only, hash-chained) =="
run log
