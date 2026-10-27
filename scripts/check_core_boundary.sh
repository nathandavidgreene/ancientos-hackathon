#!/usr/bin/env bash
# Core vs app boundary check (principle 7): core has no workspace deps,
# router depends only on core, and neither names app/transport specifics.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
deps=$(cargo metadata --format-version 1 --no-deps --offline \
  | python3 -c '
import json, sys
m = json.load(sys.stdin)
ws = {p["name"] for p in m["packages"]}
for p in m["packages"]:
    if p["name"] in ("ancientos-core", "ancientos-router"):
        d = sorted({x["name"] for x in p["dependencies"] if x["name"] in ws})
        print(p["name"] + ":" + ",".join(d))
')
core_deps=$(sed -n 's/^ancientos-core://p' <<<"$deps")
router_deps=$(sed -n 's/^ancientos-router://p' <<<"$deps")
if ! grep -q '^ancientos-core:' <<<"$deps" || ! grep -q '^ancientos-router:' <<<"$deps"; then
  echo "boundary: could not read core/router from cargo metadata" >&2
  fail=1
fi
if [ -n "$core_deps" ]; then
  echo "boundary: ancientos-core must have no workspace deps, has: $core_deps" >&2
  fail=1
fi
if [ "$router_deps" != "ancientos-core" ]; then
  echo "boundary: ancientos-router may depend only on ancientos-core, has: ${router_deps:-<none>}" >&2
  fail=1
fi

# App, transport and UI specifics that must not appear in core or router.
# The "bookmarks" ANCIENTOS_JEV_TASKS env value is kept for config
# compatibility, so the bare word is not denied; identifiers are.
deny='[Dd]iscord|web[-_ ][Uu][Ii]|bookmarks_agent|BookmarkClassification|SENSITIVE_CATEGORIES|class_of_category|Health & Personal|BRIEF_PAID|DEEPER_PAID|WIKI_EDIT|brief\.paid|deeper\.paid|wiki\.edit|default_gate|try_it_cascade|topic_filter|feedback::|muse_runs|muse-runs'
if hits=$(rg -n -e "$deny" crates/core/src crates/router/src); then
  echo "boundary: app specifics in core/router:" >&2
  echo "$hits" >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  exit 1
fi
echo "boundary: ok (core: no workspace deps; router: core only; deny-list clean)"
