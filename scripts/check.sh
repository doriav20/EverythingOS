#!/usr/bin/env bash
# Quality gate run before every commit: formatting, lints and tests.
set -euo pipefail

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace "$@"
