# Evaluation

Offline harness: `cargo run -p ancientos-eval -- --bookmarks <jsonl> --labels <jsonl> [--out <md>] [--name <name>]`. It uses no network and makes no paid calls. The output holds aggregate numbers only, with no post text, ids or urls.

- **Try it**: precision and recall of `try_it::classify` against a reference label for "a hands-on practice you could physically try".
- **Brief ranking**: `brief::select_candidates` plus `brief::local_picks` (the deterministic path used when paid calls are off) against a reference label for "worth resurfacing". This scores the local fallback, not Muse Spark's picks.

**Reference labeler: Muse Spark** (`muse-spark-1.3`). It labeled all 1,022 real bookmarks (try it, worth resurfacing, category) on 2026-10-26. Labels and post text stay outside the repo; only aggregates appear here. Only the synthetic sample (`fixtures/eval/`) is committed.

Correction: an earlier 120-post sample (seed 7), previously described as hand-labeled, was actually labeled by the local `llama3.2:1b` model, not by a person. Those numbers are kept below for history but are superseded by the Muse Spark scores.

## Try it vs Muse Spark reference labels (n=1,022, 2026-10-26)

Rules were retuned with Muse Code (log #21) on a tune split (odd lines, n=511) and checked on a held-out split (even lines, n=511) that Muse Code never read.

| Version | Set | P / R / F1 | Accuracy |
| --- | --- | --- | --- |
| Before `08f7732` | all 1,022 | 0.358 / 0.171 / 0.231 | 0.811 |
| `08f7732` | all 1,022 | 0.373 / 0.112 / 0.172 | 0.821 |
| Retuned (#21) | tune 511 | 0.910 / 0.899 / 0.904 | - |
| Retuned (#21) | **held-out 511** | **0.590 / 0.396 / 0.474** | - |
| Retuned (#21) | all 1,022 | 0.770 / 0.629 / 0.693 | 0.907 |
| Retuned (#21) | synthetic 24 | 1.000 / 0.900 / 0.947 | - |
| CV retune (#22) | tune 511 | 0.743 / 0.987 / 0.848 | - |
| CV retune (#22) | **held-out 511** | **0.524 / 0.473 / 0.497** | - |
| CV retune (#22) | all 1,022 | 0.647 / 0.712 / 0.678 | 0.887 |
| CV retune (#22) | synthetic 24 | 1.000 / 0.900 / 0.947 | - |

The held-out row is the honest number; the tune split overfits. Muse Spark tags more posts as Try it (170 of 1,022) than the earlier llama labels did, including software setups and runnable prompts, so the earlier gain did not carry over.

Run #22 used 5-fold cross-validation inside the tune half, broader cues, and pruned phrases that matched a single bookmark. Held-out F1 rose only to 0.497: the 5 folds all score 0.80-0.87, but Muse Code studied errors across every fold, so the folds stopped acting as unseen data. Keyword rules look capped near 0.5 on unseen posts; the next step is a learned classifier, or using Muse Spark labels at runtime when paid calls are on.

## Try it classifier: before / after (2026-10-26)

Change: structural cues (numbered/bulleted lists, durations, imperatives) no longer tag on their own, so list-style advice posts stay untagged; a practice cue (breath, hold, reps, each side, mindfulness, ingredients, practice/demo/kitchen words) is now required. Short video demos are caught: a video/GIF attachment, or a short captioned post with a link, tags when it carries an action verb or practice cue. Still local and deterministic.

| Sample | Before P / R / F1 | After P / R / F1 | Accuracy before → after |
| --- | --- | --- | --- |
| Real (n=120, llama3.2:1b labels, superseded) | 0.308 / 0.400 / 0.348 | 0.778 / 0.700 / 0.737 | 0.875 → 0.958 |
| Synthetic (n=24) | 0.889 / 0.800 / 0.842 | 1.000 / 0.900 / 0.947 | 0.875 → 0.958 |

Remaining real errors (aggregate): 3 misses are long self-help/practice posts without physical cues; 2 false tags are short link posts about AI agents. The real fixture carries no media field, so the video rule is exercised by tests and live X data, not this sample. Labels were from llama3.2:1b, not a person; superseded by the Muse Spark section above.

## Eval report — real bookmarks, llama3.2:1b-labeled sample (n=120, superseded)

Labels: 120 total, 120 matched, 0 unmatched.

| Task | n | TP | FP | FN | TN | Precision | Recall | F1 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| try-it | 120 | 7 | 2 | 3 | 108 | 0.778 | 0.700 | 0.737 |

Try-it accuracy: 0.958

| Ranking metric | Value |
| --- | --- |
| Local picks (n) | 5 |
| Precision@5 | 0.600 |
| Candidates (n) | 20 |
| Candidate precision | 0.350 |
| Candidate recall | 0.163 |
| Brief-worthy base rate | 0.358 |
| Brief-worthy (n) | 43 |
| Topic coverage | 11 |

## Eval report — synthetic sample (n=24, committed)

Labels: 24 total, 24 matched, 0 unmatched.

| Task | n | TP | FP | FN | TN | Precision | Recall | F1 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| try-it | 24 | 9 | 0 | 1 | 14 | 1.000 | 0.900 | 0.947 |

Try-it accuracy: 0.958

| Ranking metric | Value |
| --- | --- |
| Local picks (n) | 5 |
| Precision@5 | 0.600 |
| Candidates (n) | 20 |
| Candidate precision | 0.450 |
| Candidate recall | 1.000 |
| Brief-worthy base rate | 0.375 |
| Brief-worthy (n) | 9 |
| Topic coverage | 20 |

## Ranking by meaning (synthetic, n=24 votes, 2026-10-27)

Leave-later-out by vote time: 16 earlier votes train, the later 8 posts (4 brief-worthy) are scored. Vectors from `all-minilm` (local Ollama) are cached in `fixtures/eval/synthetic_vectors.json`, so CI and the web UI's `/accuracy` panel never need the model.

| Picks | P@5 |
| --- | --- |
| Today's order | 0.600 |
| Votes (personal ranker) | 0.400 |
| Meaning (local embeddings) | 0.600 |
| Votes + meaning (blend, weight 0.5) | 0.600 |

Blending meaning in recovers the drop from votes alone on this set but does not beat today's order; 8 test posts is too few to claim more. Reproduce: `ancientos-eval --bookmarks fixtures/eval/synthetic_bookmarks.jsonl --labels fixtures/eval/synthetic_labels.jsonl --verdicts fixtures/eval/synthetic_verdicts.jsonl --vectors fixtures/eval/synthetic_vectors.json`.

## Reading the numbers

- Try it on real posts: 7 of 10 hands-on posts found, 7 of 9 tags correct (F1 0.74, up from 0.35).
- Local brief picks are worth resurfacing in 3 of 5 (P@5 0.60) against a base rate of 0.36, so the ordering beats chance. Candidate recall is low by design: only 20 of 120 posts are kept, sorted by recency and spread across topics.
