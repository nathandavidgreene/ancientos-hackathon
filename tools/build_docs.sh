#!/usr/bin/env bash
# Build the docs site: rustdoc + generated module pages + MkDocs -> site/.
# Needs mkdocs, mkdocs-material and mkdocs-mermaid2-plugin on PATH.
# KEEP_GENERATED_DOCS=1 keeps docs/api/ and docs/reference/ afterwards.
set -euo pipefail
cd "$(dirname "$0")/.."

target_dir="${CARGO_TARGET_DIR:-target}"
cargo doc --no-deps --workspace --document-private-items

rm -rf docs/api
mkdir -p docs/api
cp -r "$target_dir/doc/." docs/api/
# Workspace landing page for the rustdoc tree (cargo doc writes none).
{
  echo '<!doctype html><meta charset="utf-8"><title>AncientOS API reference</title>'
  echo '<style>body{font-family:sans-serif;max-width:40em;margin:2em auto}</style>'
  echo '<h1>AncientOS API reference (rustdoc)</h1><ul>'
  for d in docs/api/*/; do
    name="$(basename "$d")"
    [ -f "$d/index.html" ] || continue
    case "$name" in src|static.files|search.desc|trait.impl|type.impl|implementors) continue ;; esac
    echo "<li><a href=\"$name/index.html\">$name</a></li>"
  done
  echo '</ul>'
} > docs/api/index.html

python3 tools/gen_module_pages.py

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdocs build --clean --site-dir "$tmp/site"
mkdir -p site
if command -v rsync >/dev/null 2>&1; then
  rsync -a --delete "$tmp/site/" site/
else
  rm -rf site && cp -r "$tmp/site" site
fi

if [ "${KEEP_GENERATED_DOCS:-0}" != "1" ]; then
  rm -rf docs/api docs/reference
fi
echo "docs built: site/index.html"
