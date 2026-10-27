# ancientos-hackathon
AncientOS rebuild for the Meta hackathon \(Oct 26-Nov 4, 2026\)

## What this is
AncientOS hackathon build (Oct 26 - Nov 4 2026): a Rust core with a scheduler,
a fail-closed approval gate and an append-only, hash-chained decision log; a
privacy router (sensitive data -> local Llama via Ollama/llama.cpp,
sanitized -> Muse Spark / Wonder via the Meta Model API); and an X bookmarks
agent (read-only OAuth2 PKCE, incremental ingest that never infers removals).

## Layout
- `crates/core` - scheduler, approval gate, decision log
- `crates/router` - privacy router and model clients (Muse, local LLM)
- `crates/bookmarks-agent` - X bookmark ingest, fixture loader, CLI
- `crates/web-ui` - loopback-only demo web UI (brief voting, ideas, try-it, log viewer)
- `fixtures/sample_bookmarks.jsonl` - 5 public sample bookmarks

## Run
```sh
cp .env.example .env   # fill in locally; never commit
scripts/ci_local.sh    # fmt, clippy, tests
cargo run -p bookmarks-agent -- load fixtures/sample_bookmarks.jsonl
```

## Post liveness
Before a post surfaces in the brief or web-ui, `bookmarks-agent` confirms it
still exists via the existing batched X lookup (`/tweets?ids=`, up to 100 per
request). Deleted posts, suspended/deleted authors and protected posts are
dropped silently — only a 24h on-disk cache entry (`liveness_cache.json`) is
recorded, never a decision-log entry. Transient errors fail closed (skipped,
not cached); `brief --offline` / `--demo`, `ANCIENTOS_OFFLINE=1` and paid-off
skip the check with no network. X is read-only (never un-bookmark) and stored
bookmarks are never deleted.

## Launch web UI
```sh
cargo run -p web-ui -- --demo   # temp data dir seeded with the scripts/demo.sh fixtures
```
Then open http://127.0.0.1:8787/ in a browser (loopback only, server-rendered HTML, works without JS). Use `--data DIR` instead of `--demo` to serve your own data dir; `--port` and loopback-only `--host` are optional.
