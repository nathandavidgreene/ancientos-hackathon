#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
scripts/check_core_boundary.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
