# Demo storyboard — 3:00 video script

Rubric: Impact 30 · Meta tooling 25 · Technical depth 20 · Demo 25.
Rules: show only what is built and tested. All live segments run `scripts/demo.sh`
on synthetic fixtures (`fixtures/demo/`) with mock clients and `ANCIENTOS_ALLOW_PAID`
unset. No real account, tokens, bookmarks, inspiration notes or decision logs on screen.
Terminal: large font, clean prompt (`$`), no hostname/home path visible.

## Shot list

| # | Time | VO (narration) | On-screen action | Rubric |
|---|------|----------------|------------------|--------|
| 1 | 0:00-0:12 | "I save hundreds of posts on X. I almost never read them again." | Title card "Saved ≠ read". Animated counter of synthetic bookmarks ticking up. | Impact |
| 2 | 0:12-0:25 | "One of them, a Word clone written in Rust, convinced me to rebuild my personal OS in Rust. This is that rebuild." | Public WordCraft post card (blurred handle ok) → cut to repo tree `crates/core`, `router`, `bookmarks-agent`. | Impact |
| 3 | 0:25-0:40 | "AncientOS: a governed agent core. Nothing acts without passing a fail-closed gate, and every decision lands in an append-only, hash-chained log." | Architecture diagram: scheduler → gate → decision_log; privacy router splitting to Llama (local/Modal) and Muse Spark. | Technical depth |
| 4 | 0:40-0:55 | "Sync is read-only and incremental. It never deletes, and never assumes a bookmark was removed." | `scripts/demo.sh` step 1: sync output, "N new, 0 removed (removals never inferred)". | Technical depth, Demo |
| 5 | 0:55-1:20 | "Each morning it writes a brief: three to five picks, full text, threads unrolled, images inline." | Demo brief: one pick with blockquoted full text, unrolled thread, media thumbnail under the text. | Demo, Impact |
| 6 | 1:20-1:35 | "Sensitive categories never reach the cloud model. They're routed to local Llama; only sanitized text goes to Muse Spark." | Brief log lines `-> LocalLlama` / `-> MuseSpark`; flash test name `sensitive_data_never_reaches_muse` passing. | Meta tooling, Technical depth |
| 7 | 1:35-1:55 | "I vote: approve, skip, never this post, never this category, or hand it to the agent. Three votes at eighty percent and a category auto-approves." | Run `vote` on 3 synthetic picks → `threshold shift: +["Rust & Systems"]` → next brief shows `[AUTO-APPROVED]`. | Demo, Impact |
| 8 | 1:55-2:15 | "Hand it to the agent and it unrolls the thread and turns it into an idea card, mapped onto AncientOS. Bookmark, to idea, to roadmap." | `delegate` on a synthetic post → idea card (source, idea, mapping: gate/decision_log, effort, status=new) → weekly triage adopt → backlog item. | Impact, Demo |
| 9 | 2:15-2:25 | "Hands-on posts get a 'Try it' tag with steps, and a next-day check-in." | Brief pick tagged `[TRY IT]` with numbered steps; follow-up prompt "Did you try it?". | Impact |
| 10 | 2:25-2:35 | "And every one of those decisions is provable." | `demo.sh` final step: `hash chain verified`; then tamper one line → verification fails closed. | Technical depth |
| 11 | 2:35-2:50 | "Built with Meta: Muse Code wrote most of this codebase, every run logged. Muse Spark writes the briefs." | Scroll `docs/muse-code-log.md` table; overlay run count, test count, tokens per brief (~1.6k). | Meta tooling |
| 12 | 2:50-3:00 | "This is the foundation. Bookmarks is the first agent; the rest of AncientOS plugs into the same core." | Roadmap slide: more agents on core, custom X feed ranking, go-deeper sessions. End card: repo URL. | Impact |

## Recording checklist
- Run `scripts/demo.sh` from a clean clone; confirm no paid calls (switch off).
- Verify every on-screen string above exists in actual output; drop any shot not yet built (media, Try it, idea cards are in progress).
- Pre-record any live-API clip only with Nathan's approval and with handles/tokens hidden.
- Keep total ≤ 3:00; shots 9 and 12 are the first to trim.

## Open items before recording
- Wire Modal Llama endpoint (stub today; fails closed with paid switch off).
- Synthetic fixtures for media, Try it and idea-card shots.
- Storyboard locked by Oct 30 gate.
