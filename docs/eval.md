# Evaluation

Offline harness: `cargo run -p eval -- --bookmarks <jsonl> --labels <jsonl> [--out <md>] [--name <name>]`. It uses no network and makes no paid calls. The output holds aggregate numbers only, with no post text, ids or urls.

- **Try it**: precision and recall of `try_it::classify` against a hand label for "a hands-on practice you could physically try".
- **Brief ranking**: `brief::select_candidates` plus `brief::local_picks` (the deterministic path used when paid calls are off) against a hand label for "worth resurfacing". This scores the local fallback, not Muse Spark's picks.

The labels for the real bookmarks are kept outside the repo. They cover a random sample of 120 (seed 7), hand-labeled by Flint on 2026-10-26, so expect noise from a single labeler. Only the synthetic sample (`fixtures/eval/`) is committed.

## Eval report — real bookmarks, hand-labeled sample (n=120, labels private)

Labels: 120 total, 120 matched, 0 unmatched.

| Task | n | TP | FP | FN | TN | Precision | Recall | F1 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| try-it | 120 | 4 | 9 | 6 | 101 | 0.308 | 0.400 | 0.348 |

Try-it accuracy: 0.875

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
| try-it | 24 | 8 | 1 | 2 | 13 | 0.889 | 0.800 | 0.842 |

Try-it accuracy: 0.875

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

## Reading the numbers

- On real posts, the Try it classifier finds 4 of 10 hands-on posts (recall 0.40), and 9 of its 13 tags are false positives. It over-triggers on list-style advice posts and misses short video posts. Next step: a local Llama second pass on candidate tags, scored by this harness.
- Local brief picks are worth resurfacing in 3 of 5 (P@5 0.60) against a base rate of 0.36, so the ordering beats chance. Candidate recall is low by design: only 20 of 120 posts are kept, sorted by recency and spread across topics.
