# Crates

One page per crate, generated at build time by `tools/gen_module_pages.py` from
each file's `//!` module doc and each public item's `///` doc. Every module
section links to its full [rustdoc](api.md) page.

- [ancientos-core](reference/ancientos-core.md): `approvals`, `gate`, `log`, `loop_guard`, `redact`, `scheduler`, plus the read-only `ancientos-log` CLI (verify, show, replay-approvals)
- [ancientos-router](reference/ancientos-router.md): `data_class`, `dispatch`, `jev_client`, `local_llm`, `modal`, `muse`
- [bookmarks-agent](reference/bookmarks-agent.md): `author_filter`, `brief`, `callback`, `daemon`, `deeper`, `delegate`, `digest`, `feedback`, `fixture`, `idea_card`, `ingest`, `language`, `link_context`, `liveness`, `media`, `merge`, `oauth`, `ranker`, `sanitize`, `semantic`, `stats`, `token_store`, `topic_filter`, `translate`, `try_it`, `try_it_cascade`, `unroll`, `vote`, `wiki`, `x_api`, plus the CLI binary
- [web-ui](reference/web-ui.md): `phone` (mobile approve/deny page at `/m`, token-gated)
- [muse-runs](reference/muse-runs.md): second app, a Muse Code run as a gated action, plus the CLI binary
- [demo-runner](reference/demo-runner.md)
- [ancientos-eval](reference/ancientos-eval.md): `harness`, the `eval` and `try_it_cascade` binaries
- [Python build tools](reference/python-tools.md): the scripts that build this site

See [Architecture](architecture.md) for each module's role in one line.
