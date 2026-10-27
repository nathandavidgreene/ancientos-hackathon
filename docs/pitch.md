# AncientOS: a governed bookmarks agent, built with Meta

## Problem
People save posts on X to read "later" and later never comes. Bookmarks pile into
the thousands, good ideas are lost, and handing the pile to a generic AI agent
means giving an unaccountable process your private reading history.

## Solution
A Rust rebuild of AncientOS, a personal agent OS, with an X bookmarks agent as its
first citizen:
- **Daily brief**: 3-5 picks with full post text, unrolled author threads, quoted posts and media.
- **Verdicts that teach it**: approve / skip / never_post / never_category / delegate. A category auto-approves after ≥3 votes at ≥80%.
- **Delegate → idea cards**: the agent unrolls the thread and writes an idea card (source, idea, AncientOS mapping, effort, status). Weekly triage: adopt / park / drop. Bookmark → idea → roadmap.
- **"Try it"**: hands-on posts get numbered steps and a next-day "did you try it?" check.

The project's own origin story: a bookmarked Rust Word clone (WordCraft) is what
convinced the author to rebuild AncientOS in Rust.

## Built with Meta
- **Muse Code is the primary builder.** Every feature run is logged in `docs/muse-code-log.md`: Muse Spark client with fail-closed paid switch (run 1), X OAuth2 PKCE + incremental sync (run 2, 54 tests), daily brief with merge/sanitize/gate (run 3, 86 tests), full-text hydration (run 4, 95 tests), verdicts + unroll + delegate (run 5), thread-fetch fix (run 6). Each run took ~3.5-13 min and landed with CI green after human review. The one scaffold step not done by Muse is marked as such (run 2b).
- **Muse Spark (Meta Model API)** writes each brief in one call (~1.6k tokens live) and delegate notes (~1.1k tokens for an 8-post thread).
- **Llama** handles sensitive categories via the privacy router, so private data never reaches a cloud model.

## Technical depth
- Rust workspace: `core` (scheduler, fail-closed approval gate, append-only hash-chained decision log), `router` (privacy router, Muse/Llama clients), `bookmarks-agent`.
- **Core plus apps**: `core` and `router` hold no app code. Each app (bookmarks-agent is the first) brings its own gate policy and data rules, and the core denies anything an app has not registered. `scripts/check_core_boundary.sh` runs in CI and fails if app code leaks into the core.
- Fail closed everywhere: missing key, paid switch off (`ANCIENTOS_ALLOW_PAID` default off), or gate denial all stop the action.
- Sync never deletes and never infers removals; merge of live + fixture by post_id keeps the best label.
- Liveness: brief and web-ui confirm posts still exist via a batched read-only X lookup (deleted/suspended/protected dropped silently, 24h disk cache, offline/demo skip, transient errors fail closed); X is never written to and stored bookmarks are never deleted.
- Secrets env-only; tokens stored 0600; test `sensitive_data_never_reaches_muse` enforces routing.
- 155+ tests; `scripts/ci_local.sh` runs fmt, clippy and tests.

## Impact
Turns a write-only habit into a feedback loop: saved posts become read, rated,
tried, or turned into roadmap items, with every agent decision auditable. The
same core governs any future agent, so the pattern generalizes beyond X.

## What's next
- Full AncientOS rebuild on this core (more agents on the same gate and log).
- Custom X feed ranking from accumulated verdicts.
- "Go deeper" scheduled walkthroughs.
- Local Llama backend live via Ollama/llama.cpp; Modal stays an optional future target.
