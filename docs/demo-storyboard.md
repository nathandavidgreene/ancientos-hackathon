# Demo storyboard (draft, 3:00)

Rubric: Impact 30 · Meta tooling 25 · Technical depth 20 · Demo 25.
Rule: demo only what is built and tested. Live segments use `scripts/demo.sh`
(synthetic data, mock clients, no paid calls) unless a live switch is approved.

| Time | Scene | On screen | Rubric |
|------|-------|-----------|--------|
| 0:00-0:20 | Problem: thousands of saved X bookmarks, never reread | Bookmark count, "saved ≠ read" | Impact |
| 0:20-0:40 | Promise: a governed agent that writes a 3-5 pick daily brief and learns from you | AncientOS core diagram (scheduler, gate, decision log, privacy router) | Impact, Technical depth |
| 0:40-1:05 | Sync: incremental, read-only, never deletes | `demo.sh` step 1; real-sync numbers shown only as a recorded clip | Demo, Technical depth |
| 1:05-1:35 | Brief + privacy router: sensitive category goes to local Llama (Modal), sanitized to Muse Spark | Brief output with `-> LocalLlama` / `-> MuseSpark`; router test name `sensitive_data_never_reaches_muse` | Meta tooling, Technical depth |
| 1:35-2:05 | Feedback: approve / skip / never; threshold shifts after 3 votes at ≥80% | `threshold shift: +["Rust & Systems"]`, then day-3 brief with `[AUTO-APPROVED]` | Demo, Impact |
| 2:05-2:25 | Governance: append-only, hash-chained decision log | `demo.sh` step 5, `hash chain verified` | Technical depth |
| 2:25-2:45 | Built with Meta: Muse Code wrote the client/auth/sync (log in `docs/muse-code-log.md`), Muse Spark writes briefs, Llama handles private data | Muse Code log table, cost per run | Meta tooling |
| 2:45-3:00 | Foundation for AncientOS: generic core, more agents plug in | Roadmap slide | Impact |

## Open items before recording
- Wire Modal Llama endpoint (stub today; fails closed with paid switch off).
- Swap mock brief writer for the real Muse brief from `flint/muse-x-auth`.
- Decide whether any live paid call appears on screen (needs approval).
- Verdicts are now approve / skip / never_post / never_category / delegate; consider showing a delegate run (thread unroll -> inspiration notes).
- Backlog (media, "Try it", "go deeper", feed ranking): see `docs/backlog.md`; mention on roadmap slide only.
