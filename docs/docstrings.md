# Doc comments

## The rule

Every function has a doc comment that says what it does, not how:

- **Rust:** a `///` line directly above every `fn` (attributes and plain `//`
  comments may sit between). `#[test]` functions are exempt: the test name is
  its description. Test helpers are not exempt.
- **Shell:** every script in `scripts/` has a header comment on line 2, and
  every shell function has a `#` comment directly above it.
- **Python:** every module and function under `tools/` has a docstring.

## The check

`scripts/check_docs.sh` enforces the rule and prints coverage. It runs in
`scripts/ci_local.sh` right after the core boundary check, so CI fails on any
undocumented function. `scripts/check_docs.sh --list` prints each gap as
`path:line: signature`.

## How this site is built

`tools/build_docs.sh`:

1. `cargo doc --no-deps --workspace --document-private-items` into `docs/api/`.
2. `tools/gen_module_pages.py` writes `docs/reference/`: one page per crate,
   the CLI reference from each binary's `//!` doc, and the Python tools page.
3. `mkdocs build` (Material theme, mermaid2 plugin) into `site/`.

Generated folders (`site/`, `docs/api/`, `docs/reference/`) are git-ignored.
Needs `mkdocs`, `mkdocs-material` and `mkdocs-mermaid2-plugin`; nothing is
published.
