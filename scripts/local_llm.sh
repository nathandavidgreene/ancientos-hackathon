#!/usr/bin/env bash
# Local LLM (Ollama) lifecycle for sensitive-data inference.
#   start  - launch `ollama serve` if the API is down, then pull the model if missing
#   health - check the API is up and the model is present (no changes)
#   ensure - idempotent start (restarts only when down)
# Env overrides: OLLAMA_BIN, OLLAMA_DIR, ANCIENTOS_LOCAL_LLM_URL, ANCIENTOS_LOCAL_LLM_MODEL.
set -euo pipefail

OLLAMA_BIN="${OLLAMA_BIN:-$HOME/.local/ollama/bin/ollama}"
OLLAMA_DIR="${OLLAMA_DIR:-$HOME/.local/ollama}"
SERVE_LOG="$OLLAMA_DIR/serve.log"
BASE_URL="${ANCIENTOS_LOCAL_LLM_URL:-http://localhost:11434}"
MODEL="${ANCIENTOS_LOCAL_LLM_MODEL:-llama3.2:1b}"
BASE_URL="${BASE_URL%/}"
API_BASE="${BASE_URL%/v1}"
TAGS_URL="$API_BASE/api/tags"
START_TIMEOUT_SECS=30
CURL_TIMEOUT_SECS=3

# Print an error and exit 1.
die() { echo "error: $*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl not found; install curl to manage the local LLM"

# --noproxy: loopback health checks must not depend on proxy config.
api_up() { curl -sf --noproxy '*' --max-time "$CURL_TIMEOUT_SECS" "$TAGS_URL" >/dev/null 2>&1; }

# Fetch the Ollama model list as JSON.
tags_json() { curl -sf --noproxy '*' --max-time "$CURL_TIMEOUT_SECS" "$TAGS_URL" 2>/dev/null; }

# True when the configured model is already pulled.
model_present() { tags_json 2>/dev/null | grep -q -F "\"name\":\"$MODEL\""; }

# Pull the configured model.
pull_model() {
  if model_present; then
    echo "model $MODEL present at $BASE_URL"
  else
    echo "pulling model $MODEL..."
    "$OLLAMA_BIN" pull "$MODEL" || die "failed to pull model $MODEL"
  fi
}

# start: launch the server if it is down, then pull the model if missing.
cmd_start() {
  if api_up; then
    echo "local LLM already running at $BASE_URL"
  else
    [[ -x "$OLLAMA_BIN" ]] || die "ollama binary not found at $OLLAMA_BIN (expected ~/.local/ollama/bin/ollama)"
    mkdir -p "$OLLAMA_DIR"
    echo "starting ollama serve (log: $SERVE_LOG)..."
    "$OLLAMA_BIN" serve >>"$SERVE_LOG" 2>&1 &
    local waited=0
    while ! api_up && [[ $waited -lt $START_TIMEOUT_SECS ]]; do
      sleep 1
      waited=$((waited + 1))
    done
    api_up || die "ollama serve did not respond at $TAGS_URL within ${START_TIMEOUT_SECS}s; see $SERVE_LOG"
    echo "local LLM responding at $BASE_URL"
  fi
  pull_model
  echo "local LLM ready: $BASE_URL ($MODEL)"
}

# health: check the API is up and the model is present, changing nothing.
cmd_health() {
  api_up || die "local LLM unavailable at $TAGS_URL; run scripts/local_llm.sh start (sensitive data was NOT sent elsewhere)"
  model_present || die "model $MODEL not present at $BASE_URL; run scripts/local_llm.sh start to pull it"
  echo "local LLM healthy: $BASE_URL ($MODEL)"
}

# ensure: idempotent start; restarts only when the API is down.
cmd_ensure() {
  if api_up; then
    echo "local LLM already running at $BASE_URL"
    pull_model
    echo "local LLM ready: $BASE_URL ($MODEL)"
  else
    echo "local LLM down; starting..."
    cmd_start
  fi
}

case "${1:-}" in
  start) cmd_start ;;
  health) cmd_health ;;
  ensure) cmd_ensure ;;
  *) echo "usage: scripts/local_llm.sh start|health|ensure" >&2; exit 2 ;;
esac
