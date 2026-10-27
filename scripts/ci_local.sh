#!/usr/bin/env bash
# Full local CI: core boundary, doc coverage, strict Pyright, fmt, clippy (-D warnings), tests, rustdoc (-D warnings).
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/check_core_boundary.sh
scripts/check_docs.sh
scripts/check_pyright.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --document-private-items --quiet
