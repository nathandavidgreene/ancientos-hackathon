# Demo storyboard — 3:00 video script

Rubric: Impact 30 · Meta tooling 25 · Technical depth 20 · Demo 25.
Rules: show only what is built and tested. All live segments run `scripts/demo.sh`
on synthetic fixtures (`fixtures/demo/`) with mock clients and `ANCIENTOS_ALLOW_PAID`
unset. No real account, tokens, bookmarks, inspiration notes or decision logs on screen.
Terminal: large font, clean prompt (`$`), no hostname/home path visible.
Every backticked string in the On-screen action column is quoted verbatim from
`scripts/demo.sh` output; `crates/demo-runner/tests/demo_script.rs` fails if any drifts.

## Shot list

| # | Time | VO (narration) | On-screen action | Rubric |
|---|------|----------------|------------------|--------|
| 1 | 0:00-0:12 | "I save hundreds of posts on X. I almost never read them again." | Terminal shows `==== Shot 1/12: Saved != read ====` then `synthetic bookmarks: 20 (15 + 5)` with `synthetic_bookmarks.jsonl: 15` and `try_it_and_thread_bookmarks.jsonl: 5`. | Impact |
| 2 | 0:12-0:25 | "One of them, a Word clone written in Rust, convinced me to rebuild my personal OS in Rust. This is that rebuild." | Terminal shows `==== Shot 2/12: Rust rebuild ====`, `WordCraft (public post, handle blurred) -> repo tree:` then `crates/core`, `crates/router`, `crates/bookmarks-agent`, `crates/demo-runner`. | Impact |
| 3 | 0:25-0:40 | "AncientOS: a governed agent core. Nothing acts without passing a fail-closed gate, and every decision lands in an append-only, hash-chained log." | Terminal shows `==== Shot 3/12: Governed agent core ====` then `scheduler -> gate -> decision_log`, `privacy router: LocalLlama (local, sensitive) / MuseSpark (sanitized)`, `fail-closed gate; append-only, hash-chained decision log`. | Technical depth |
| 4 | 0:40-0:55 | "Sync is read-only and incremental. It never deletes, and never assumes a bookmark was removed." | Terminal shows `==== Shot 4/12: Read-only sync ====` then `synced 15 new, 0 removed (removals never inferred) (synthetic fixture)` and `synced 5 new, 0 removed (removals never inferred) (synthetic fixture)`. | Technical depth, Demo |
| 5 | 0:55-1:20 | "Each morning it writes a brief: three to five picks, full text, threads unrolled, images inline." | Terminal shows `==== Shot 5/12: Morning brief ====` then `Daily brief — 2026-10-26 (day 3):` with blockquoted full text `> [synthetic] Box breathing: inhale 4s, hold 4s, exhale 4s, hold 4s. Repeat 4 rounds.`, `unrolled thread demo-2001:` with `demo-2001 [synthetic] 1/3 How I run my week with a kanban board that agents can pick cards from. Thread.`, and `Media for demo-2102:` with `**Media**` and `- Media (video): [![synthetic Animated square tracing a 4-4-4-4 breathing rhythm](https://pbs.twimg.com/ext_tw_video_thumb/demo_breathing_placeholder.jpg)](https://pbs.twimg.com/ext_tw_video_thumb/demo_breathing_placeholder.jpg)`. | Demo, Impact |
| 6 | 1:20-1:35 | "Sensitive categories never reach the cloud model. They're routed to local Llama; only sanitized text goes to Muse Spark." | Terminal shows `==== Shot 6/12: Privacy routing ====` then `Health & Personal  -> LocalLlama` and `Rust & Systems     -> MuseSpark`, then `test sensitive_data_never_reaches_muse ... ok`. | Meta tooling, Technical depth |
| 7 | 1:35-1:55 | "I vote: approve, skip, never this post, never this category, or hand it to the agent. Three votes at eighty percent and a category auto-approves." | Terminal shows `==== Shot 7/12: Voting and thresholds ====` then votes incl. `demo-1002: approve (Rust & Systems)` and `demo-1015: never_category (Hot Takes)`, `threshold shift: +["Rust & Systems"] -[]`, `auto-approved categories: {"Rust & Systems"}`, `Next brief (learned category is auto-approved):` with `[AUTO-APPROVED] [synthetic] A tiny async runtime in 300 lines.` | Demo, Impact |
| 8 | 1:55-2:15 | "Hand it to the agent and it unrolls the thread and turns it into an idea card, mapped onto AncientOS. Bookmark, to idea, to roadmap." | Terminal shows `==== Shot 8/12: Delegate to idea ====` then `unroll demo-2001:`, `delegate demo-2001 -> idea card:` with `b00ed92a5aa2d135 source=https://example.invalid/demo_threadsmith/status/demo-2001 idea=Drag a card to approve mapping=gate effort=S status=new` and `3c9c657f96a7ace6 source=https://example.invalid/demo_threadsmith/status/demo-2001 idea=Shared memory log with history mapping=decision_log effort=S status=new`, `ideas (due for triage):`, `triage b00ed92a5aa2d135 adopt -> backlog item:` and `- [idea_card b00ed92a5aa2d135] Drag a card to approve (maps to: gate; effort S; source: https://example.invalid/demo_threadsmith/status/demo-2001)`. | Impact, Demo |
| 9 | 2:15-2:25 | "Hands-on posts get a 'Try it' tag with steps, and a next-day check-in." | Terminal shows `==== Shot 9/12: Try it ====` then `demo-2101 [TRY IT] (explicit try-it, numbered steps, reps/sets, duration, each side, practice cue, imperative, step imperatives; topic 'Health & Wellness')` with `1. roll shoulders 10x`, `Did you try it? (yes/no/later)`, `try-pending:`, `tried demo-2101 yes`, `recorded yes for demo-2101`. | Impact |
| 10 | 2:25-2:35 | "And every one of those decisions is provable." | Terminal shows `==== Shot 10/12: Provable log ====` with entries incl. `{"new":15,"source":"synthetic_fixture","total":15}`, then `hash chain verified: 51 entries` (printed once by the log dump and once by the tamper check), then `tampered copy: verification failed as expected (fails closed: chain broken at line 0)`. | Technical depth |
| 11 | 2:35-2:50 | "Built with Meta: Muse Code wrote most of this codebase, every run logged. Muse Spark writes the briefs." | Terminal shows `==== Shot 11/12: Built with Meta ====` then `Muse Code runs: N (from docs/muse-code-log.md)`, where N is the run count derived from docs/muse-code-log.md with the same row rule as scripts/demo.sh, and `Muse Spark writes the briefs; tokens per brief: ~1.6k`, then scrolls the log starting at `# Muse Code run log`. | Meta tooling |
| 12 | 2:50-3:00 | "This is the foundation. Bookmarks is the first agent; the rest of AncientOS plugs into the same core." | Terminal shows `==== Shot 12/12: Roadmap ====` then `Roadmap: more agents on core, custom X feed ranking, go-deeper sessions.` and `Bookmarks is the first agent; the rest of AncientOS plugs into the same core.` | Impact |

## Recording checklist
- Run `bash scripts/demo.sh` from a clean clone with `ANCIENTOS_ALLOW_PAID` unset; confirm exit 0 and byte-identical output across two runs, and `--shot N` exits 0 for N 1..12.
- Every on-screen string in the shot table is quoted verbatim from `scripts/demo.sh` output; `crates/demo-runner/tests/demo_script.rs` fails if any quoted string drifts.
- Media, Try it, and idea-card shots are built on synthetic fixtures and covered by tests.
- Sensitive traffic stays on the local Llama backend (Ollama/llama.cpp via `ANCIENTOS_LOCAL_LLM_URL`); cloud calls fail closed with the paid switch off. Modal remains an optional future target.
- Pre-record any live-API clip only with Nathan's approval and with handles/tokens hidden.
- Keep total ≤ 3:00; shots 9 and 12 are the first to trim.

## Open items before recording
- Local Llama backend wired (Ollama/llama.cpp); Modal stays an optional future target.
- Storyboard locked by Oct 30 gate.
