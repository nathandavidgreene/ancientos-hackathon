# Evaluation

Offline harness: `cargo run -p eval -- --bookmarks <jsonl> --labels <jsonl> [--out <md>] [--name <name>]`. It uses no network and makes no paid calls. The output holds aggregate numbers only, with no post text, ids or urls.

- **Try it**: precision and recall of `try_it::classify` against a hand label for "a hands-on practice you could physically try".
- **Brief ranking**: `brief::select_candidates` plus `brief::local_picks` (the deterministic path used when paid calls are off) against a hand label for "worth resurfacing". This scores the local fallback, not Muse Spark's picks.

The labels for the real bookmarks are kept outside the repo. They cover a random sample of 120 (seed 7), hand-labeled by Flint on 2026-10-26, so expect noise from a single labeler. Only the synthetic sample (`fixtures/eval/`) is committed.

## Try it classifier: before / after (2026-10-26)

Change: structural cues (numbered/bulleted lists, durations, imperatives) no longer tag on their own, so list-style advice posts stay untagged; a practice cue (breath, hold, reps, each side, mindfulness, ingredients, practice/demo/kitchen words) is now required. Short video demos are caught: a video/GIF attachment, or a short captioned post with a link, tags when it carries an action verb or practice cue. Still local and deterministic.

| Sample | Before P / R / F1 | After P / R / F1 | Accuracy before → after |
| --- | --- | --- | --- |
| Real (n=120, private labels) | 0.308 / 0.400 / 0.348 | 0.778 / 0.700 / 0.737 | 0.875 → 0.958 |
| Synthetic (n=24) | 0.889 / 0.800 / 0.842 | 1.000 / 0.900 / 0.947 | 0.875 → 0.958 |

Remaining real errors (aggregate): 3 misses are long self-help/practice posts without physical cues; 2 false tags are short link posts about AI agents. The real fixture carries no media field, so the video rule is exercised by tests and live X data, not this sample. Single labeler; small n, so treat as directional.

## Eval report — real bookmarks, hand-labeled sample (n=120, labels private)

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

## Reading the numbers

- Try it on real posts: 7 of 10 hands-on posts found, 7 of 9 tags correct (F1 0.74, up from 0.35).
- Local brief picks are worth resurfacing in 3 of 5 (P@5 0.60) against a base rate of 0.36, so the ordering beats chance. Candidate recall is low by design: only 20 of 120 posts are kept, sorted by recency and spread across topics.
