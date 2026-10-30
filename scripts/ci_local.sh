#!/usr/bin/env bash
# Full local CI: core boundary, HTTP allowlist, doc coverage, strict Pyright, fmt, clippy (-D warnings), tests, rustdoc (-D warnings).
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/check_core_boundary.sh
# Hosted-model HTTP allowlist: crates/router/src/http.rs is the only module
# allowed to touch ureq/reqwest. All hosted model calls go through
# ModelClient::complete_with_class / check_egress (data class + egress
# allowlist + ANCIENTOS_ALLOW_PAID) and reach the network only via that
# module; any other use is a bypass and fails the build. Only
# crates/router/Cargo.toml (plus the workspace root) may declare the dep.
# grep, not rg: rg is absent on the GitHub runner. A failing grep (exit > 1)
# fails closed, like the boundary check above.
allowed_http_module="crates/router/src/http.rs"
if [ ! -f "$allowed_http_module" ]; then
  echo "http-allowlist: $allowed_http_module is missing" >&2
  exit 1
fi
set +e
http_scan=$(grep -rn --exclude-dir=target --include='*.rs' -e 'ureq::' -e 'reqwest::' -e 'extern crate ureq' -e 'extern crate reqwest' crates)
http_rc=$?
set -e
if [ "$http_rc" -gt 1 ]; then
  echo "http-allowlist: source scan failed (grep exit $http_rc)" >&2
  exit 1
fi
http_hits=""
if [ "$http_rc" -eq 0 ]; then
  http_hits=$(printf '%s\n' "$http_scan" | grep -v "^$allowed_http_module:" || true)
fi
if [ -n "$http_hits" ]; then
  echo "http-allowlist: HTTP client used outside $allowed_http_module:" >&2
  echo "$http_hits" >&2
  exit 1
fi
set +e
toml_scan=$(grep -rln --exclude-dir=target --include='Cargo.toml' -e 'ureq' -e 'reqwest' crates)
toml_rc=$?
set -e
if [ "$toml_rc" -gt 1 ]; then
  echo "http-allowlist: manifest scan failed (grep exit $toml_rc)" >&2
  exit 1
fi
toml_hits=""
if [ "$toml_rc" -eq 0 ]; then
  toml_hits=$(printf '%s\n' "$toml_scan" | grep -v '^crates/router/Cargo.toml$' || true)
fi
if [ -n "$toml_hits" ]; then
  echo "http-allowlist: HTTP client dependency outside crates/router/Cargo.toml:" >&2
  echo "$toml_hits" >&2
  exit 1
fi
echo "http-allowlist: ok (HTTP clients only in $allowed_http_module)"
scripts/check_docs.sh
scripts/check_pyright.sh
cargo fmt --all -- --check
CARGO_TARGET_DIR=target/clippy cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --document-private-items --quiet
