#!/usr/bin/env bash
# Wrap a headless Muse Code run: capture token usage + duration, compute cost,
# append a row to docs/muse-code-log.md.
# Usage: scripts/muse_run.sh <log-no> "<summary>" <prompt-file> [jsonl-out]
# Pricing (USD per 1M tokens): uncached input $1.25, cached input $0.15, output $4.25.
# Secrets: META_API_KEY must be in the environment; it is never written anywhere.
# Test hook: MUSE_RUN_JSONL=<file> skips the live run and parses that file instead.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
NO="${1:?log number}"; SUMMARY="${2:?summary}"; PROMPT="${3:?prompt file}"
OUT="${4:-${TMPDIR:-/tmp}/muse-run-$NO.jsonl}"
LOG="${MUSE_LOG:-$ROOT/docs/muse-code-log.md}"
start=$(date +%s)
if [[ -n "${MUSE_RUN_JSONL:-}" ]]; then
  OUT="$MUSE_RUN_JSONL"; status=0
else
  : "${META_API_KEY:?META_API_KEY not set (fail closed)}"
  set +e
  muse exec --json --trust-workspace --workspace "$ROOT" --prompt-file "$PROMPT" >"$OUT"
  status=$?
  set -e
fi
secs=$(( $(date +%s) - start ))
read -r IN CACHED OUTT < <(grep '"payload_type":"run.usage"' "$OUT" | python3 -c '
import json,sys
i=c=o=0
for line in sys.stdin:
    for m in json.loads(line)["payload"].get("models",[]):
        i+=m.get("input_tokens",0); c+=m.get("cached_tokens",0); o+=m.get("output_tokens",0)
print(i,c,o)')
COST=$(python3 -c "i,c,o=$IN,$CACHED,$OUTT;print(f'{((i-c)*1.25+c*0.15+o*4.25)/1e6:.2f}')")
fmt(){ python3 -c "print(f'{$1:,}')"; }
OUTCOME=$([[ $status -eq 0 ]] && echo "exit 0" || echo "exit $status (FAILED)")
printf '| %s | %s | %s | muse_run.sh: %s, ~%d min (%ds), `--trust-workspace`; tokens in/cached/out %s/%s/%s; est. cost $%s. |\n' \
  "$NO" "$(date +%F)" "$SUMMARY" "$OUTCOME" $(( (secs+30)/60 )) "$secs" \
  "$(fmt "$IN")" "$(fmt "$CACHED")" "$(fmt "$OUTT")" "$COST" >>"$LOG"
echo "muse_run: #$NO exit=$status secs=$secs in=$IN cached=$CACHED out=$OUTT cost=\$$COST jsonl=$OUT"
exit $status
