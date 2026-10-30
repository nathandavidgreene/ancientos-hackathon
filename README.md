# ancientos-hackathon
AncientOS rebuild for the Meta hackathon \(Oct 26-Nov 4, 2026\)

## Why AncientOS exists

An AI agent is only useful if it can act: sync your bookmarks, run a coding job, ask a model a question. But most agents get that power by being handed your keys and trusted to behave.

AncientOS takes a different route: the agent can act, but it never holds unrestricted authority. Four jobs are kept apart:

```
 intelligence  ->  authorization  ->  execution  ->  evidence
 (models,          (gate + your       (runs once,     (hash-chained
  privacy router)   approval)          paid switch)    decision log)
```

- **Intelligence** proposes. A privacy router keeps sensitive or unclassified text on a local Llama model; only sanitized text goes to Meta's cloud models.
- **Authorization** decides. Actions an app hasn't registered are denied. Risky ones wait for your approval, on the CLI or your phone, and the approval is bound to a hash of the exact payload.
- **Execution** is limited. Anything that costs money stays off unless `ANCIENTOS_ALLOW_PAID=1` is set.
- **Evidence** is kept. Every decision lands in an append-only, hash-chained log, checked on every read.

Three apps (an X bookmarks curator, gated Muse Code runs, and Muse Wonder questions) share this one small core, which knows nothing about any of them.

What isn't done yet: the gate is enforced by convention, not by types; the log has no cross-process lock and isn't signed; and the router can still be bypassed by code that calls a model directly. Those are next.

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
- `crates/muse-runs` - second app: a Muse Code run is a gated action (propose, approve or deny, run once)
- `crates/web-ui` - loopback-only demo web UI (brief voting, ideas, try-it, log viewer)
- `fixtures/sample_bookmarks.jsonl` - 5 public sample bookmarks

## Run
```sh
cp .env.example .env   # fill in locally; never commit
scripts/ci_local.sh    # boundary, docs, strict Pyright, fmt, clippy, tests
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
