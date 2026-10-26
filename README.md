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
- `fixtures/sample_bookmarks.jsonl` - 5 public sample bookmarks

## Run
```sh
cp .env.example .env   # fill in locally; never commit
scripts/ci_local.sh    # fmt, clippy, tests
cargo run -p bookmarks-agent -- load fixtures/sample_bookmarks.jsonl
```
