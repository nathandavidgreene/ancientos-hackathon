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

## Runner

```
cargo run -q -p bookmarks-agent --bin analyze-media -- --rows FILE.jsonl [--log PATH] [--out FINDINGS.jsonl] [--live]
```

Reads `x_bookmark_media`-shaped JSONL and builds one request per post. The
default is a **dry run**: it prints items to send and keys it would skip,
with no model call and no log write. `--live` first checks
`ANCIENTOS_ALLOW_PAID=1` and `ANCIENTOS_MEDIA_MAX_ITEMS` (cap applies to the
whole run), then the live backend env. It then runs `analyze` and
`propose_store` per post on the shared decision log. Decide the resulting
`media.findings.store` approvals with the usual approve/deny tools.

## Live backend

`analyze_media_live::LiveBackend` reuses the Muse client (key read from
`META_API_KEY` per call, never stored). It requires `ANCIENTOS_MEDIA_MODEL`
and an https `ANCIENTOS_MEDIA_API_BASE`, with no defaults, and fails closed
when either is missing. It sends one chat call per item with the image URL
attached. The reply must be JSON only, `{"findings":[{kind,text,quote?,region?}]}`,
with no unknown fields. More than 20 findings keeps the first 20 in the model's
order. Anything else skips the item with a logged reason. Evidence is then enforced by the same quote-or-region check.
Tested only with synthetic replies.

## muse exec backend (default)

The Meta key works only through the Muse Code CLI, so the runner's default
backend (`ANCIENTOS_MEDIA_BACKEND=muse_exec`, or `http` for the HTTP client)
is `analyze_media_muse::MuseExecBackend`. For each item it downloads the image
into a fresh temp dir as `image.<ext>`, with no URL in the prompt. It then runs
`muse exec --json --reasoning-effort low --contributor-mode off --workspace
<tmp> --trust-workspace --image <file> <prompt>` and takes the reply from the
`run.terminal.completed` event. A run that uses tools or does not complete is
an error. Usage from `run.usage` adds to a running total. Once the total reaches
`ANCIENTOS_MEDIA_MAX_TOKENS` (required), later items fail before any call.
`ANCIENTOS_MUSE_BIN` overrides the binary path.

## Baseten backend

`ANCIENTOS_MEDIA_BACKEND=baseten` calls Muse Spark Open
(`meta-models/muse-spark-open-1026`) on Baseten's OpenAI-compatible API. It
needs `ANCIENTOS_ALLOW_PAID=1`, `BASETEN_API_KEY` (sent as `Api-Key`, never
logged), `ANCIENTOS_MEDIA_MODEL`, an https `ANCIENTOS_MEDIA_API_BASE`
(`https://inference.baseten.co/v1`) and `ANCIENTOS_MEDIA_MAX_TOKENS`; any
missing value fails closed. It sends `reasoning_effort: low`, reads
`message.content` (not the reasoning), and treats a reply cut off at the token
limit as an error. In a live one-image test it returned the same letter
findings as Muse with about 1.6K input tokens (Muse: about 29K) in about 5s.

## Thumbnails

`bookmarks-agent::thumbs` makes sharp 240px thumbnails from the stored
bookmark media (same `media.rs` model: photos use the image itself, videos
and GIFs prefer the poster, else the first frame via local `/usr/bin/ffmpeg`):

```
cargo run -q -p bookmarks-agent --bin thumbs -- [--rows FILE] [--out-dir DIR]
```

240px on the long edge (Lanczos3 plus a light unsharp mask, never upscaled),
JPEG quality 85, or PNG when the source has alpha. Output defaults to
`~/.ancientos-hackathon/eval-private/thumbs/` (dir 0700, files 0600,
overridable by `--out-dir` / `ANCIENTOS_THUMBS_DIR`), with a `manifest.jsonl`
of `{post_id, index, kind, path, width, height}` or `{..., skipped: reason}`
per item — no post text. Muted posts are skipped via the shared
`topic_mute` list (fail closed when unreadable), downloads are incremental
and bounded to X media hosts only, and no paid calls are made.

Media rows must include original `full_text` and, when applicable, `quoted_text`.
Missing post context fails closed. Post text, quoted text, and alt text are
screened against mutes before resolution or any backend call.
