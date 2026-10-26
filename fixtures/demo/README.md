# Demo fixtures (synthetic)

All content here is invented. Handles are `demo_*`, URLs use `example.invalid`, media URLs are placeholder paths on pbs.twimg.com that do not exist. No private data.

- `synthetic_bookmarks.jsonl` - base demo bookmarks.
- `try_it_and_thread_bookmarks.jsonl` - storyboard shots 8 and 9:
  - `demo-2001..2003`: 3-post thread by `@demo_threadsmith` sharing `conversation_id` `demo-2001` (with `in_reply_to_post_id`), for `unroll` + `delegate` (shot 8).
  - `demo-2101` (stretch) and `demo-2102` (breathing): hands-on posts with `media` [{type, url, alt_text}] for media in briefs and the `[TRY IT]` tag (shot 9).
- `expected_idea_cards.jsonl` - sample idea cards from delegating the thread. Fields: id, source_url, idea, ancientos_mapping, effort (S/M/L), status (new/adopted/parked/dropped), created_at, park_until (set only when parked, ~1 month out).
