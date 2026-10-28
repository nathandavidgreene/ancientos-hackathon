# analyze_media (bookmark images and video posters)

Module: `bookmarks-agent::analyze_media`. Status: built and tested with a mock
backend only; no live model is wired and it has never made a paid call.

**Input** matches the planned `x_bookmark_media` table: `post_id`, `media_key`,
`type`, `url` (photo), `preview_image_url` (video/GIF poster), `variants[]`,
`width`, `height`, `duration_ms`, `alt_text`. A request is a `post_id` plus its
`attachments.media_keys`; keys resolve by `media_key`. Photos use `url`;
videos and GIFs use the poster as `frame_idx: 0`. A key with no row, no URL,
or a non-X-media host is skipped and logged as `media.skipped`.

**Findings** are `text`, `claim`, or `description`, each with provenance
`{post_id, media_key, media_url, frame_idx?}`. A `text` or `claim` finding with
no OCR quote and no valid region box is rejected and logged as
`media.finding_rejected` (fail closed).

**Paid calls** need `ANCIENTOS_ALLOW_PAID=1` and a per-run cap
`ANCIENTOS_MEDIA_MAX_ITEMS` (1–200). A run with more items than the cap is
refused before any call.

**Storage**: nothing is written to LifeVault here. `propose_store` records a
`media.findings.store` approval on the hash-chained log; `release_store`
returns the findings only after a human approves that exact payload, once.

No segmentation model (SAM) is used.
